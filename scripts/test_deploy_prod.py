import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import deploy_prod


class DeploymentTests(unittest.TestCase):
    def run_deploy(self, conclusion):
        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory) / 'summary'
            env = {'GITHUB_REPOSITORY': 'nearai/cloud-api', 'GITHUB_RUN_ID': '123',
                   'GITHUB_RUN_ATTEMPT': '2', 'GITHUB_STEP_SUMMARY': str(summary)}
            responses = ['', '[{"databaseId": 9, "displayTitle": "unrelated"}]',
                         '[{"databaseId": 42, "displayTitle": "Update Cloud API Prod [nearai/cloud-api/123/2]"}]',
                         '{"status": "in_progress", "conclusion": null}',
                         '{"status": "completed", "conclusion": "' + conclusion + '"}']
            with patch.dict(os.environ, env), patch.object(deploy_prod, 'gh', side_effect=responses) as gh, patch.object(deploy_prod.time, 'sleep'):
                if conclusion == 'success':
                    deploy_prod.deploy()
                else:
                    with self.assertRaisesRegex(RuntimeError, conclusion):
                        deploy_prod.deploy()
                self.assertIn('/actions/runs/42', summary.read_text())
                self.assertIn('release_id=nearai/cloud-api/123/2', gh.call_args_list[0].args)
                self.assertEqual(gh.call_args_list[-1].args[2], '42')

    def test_waits_for_correlated_run(self):
        self.run_deploy('success')

    def test_propagates_failure_and_cancellation(self):
        for conclusion in ('failure', 'cancelled', 'timed_out'):
            with self.subTest(conclusion=conclusion):
                self.run_deploy(conclusion)

    @patch.object(deploy_prod.time, 'sleep')
    @patch.object(deploy_prod.time, 'monotonic', side_effect=[0, 0, 0, 301])
    @patch.object(deploy_prod, 'gh', side_effect=['', '[]'])
    def test_missing_run_times_out(self, *_):
        with patch.dict(os.environ, {'GITHUB_REPOSITORY': 'nearai/cloud-api', 'GITHUB_RUN_ID': '123', 'GITHUB_RUN_ATTEMPT': '1'}):
            with self.assertRaisesRegex(TimeoutError, 'five minutes'):
                deploy_prod.deploy()


if __name__ == '__main__':
    unittest.main()
