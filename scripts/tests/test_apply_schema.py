"""Exercise image selection and error propagation without a database or Docker daemon."""

import os
import pathlib
import subprocess
import tempfile
import unittest

from test_schema_pin import pinned_image

ROOT = pathlib.Path(__file__).resolve().parents[2]
TEST_IMAGE = "ghcr.io/nearai/cloud-platform@sha256:" + "a" * 64


class ApplySchemaTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = pathlib.Path(self.temp.name)
        self.calls = self.directory / "docker-calls"
        docker = self.directory / "docker"
        docker.write_text(
            "#!/usr/bin/env bash\n"
            'printf "%s\\n" "$*" >> "$DOCKER_CALLS"\n'
            'case "$1" in\n'
            '  info) echo Linux ;;\n'
            '  image) exit 0 ;;\n'
            '  run) cat >/dev/null; exit "${MIGRATION_EXIT:-0}" ;;\n'
            '  *) echo "unexpected docker call" >&2; exit 99 ;;\n'
            "esac\n",
            encoding="utf-8",
        )
        docker.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{self.directory}:{os.environ['PATH']}",
            "DOCKER_CALLS": str(self.calls),
            "PGHOST": "localhost",
            "PGPORT": "5432",
            "PGDATABASE": "schema_test",
            "PGUSER": "postgres",
            "PGPASSWORD": "test-only",
        }
        for name in ("APPLY_SCHEMA_SKIP", "CLOUD_PLATFORM_TEST_IMAGE", "SCHEMA_DOCKER_NETWORK"):
            self.env.pop(name, None)

    def run_schema(self, **env):
        return subprocess.run(
            ["bash", str(ROOT / "scripts/apply-schema.sh")],
            env={**self.env, **env},
            text=True,
            capture_output=True,
            check=False,
        )

    def test_default_uses_normal_pin(self):
        result = self.run_schema()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"{pinned_image()} migrate", self.calls.read_text())

    def test_companion_digest_runs_the_migrator(self):
        result = self.run_schema(CLOUD_PLATFORM_TEST_IMAGE=TEST_IMAGE)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls.read_text()
        self.assertIn(f"{TEST_IMAGE} migrate", calls)
        self.assertNotIn(pinned_image(), calls)

    def test_mutable_or_other_package_images_fail_before_docker(self):
        for image in (
            "ghcr.io/nearai/cloud-platform:ci-pr-113",
            "ghcr.io/nearai/cloud-platform@sha256:abc",
            "ghcr.io/other/cloud-platform@sha256:" + "a" * 64,
        ):
            with self.subTest(image=image):
                result = self.run_schema(CLOUD_PLATFORM_TEST_IMAGE=image)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must pin", result.stderr)
                self.assertFalse(self.calls.exists())

    def test_failed_companion_migration_does_not_fall_back(self):
        result = self.run_schema(CLOUD_PLATFORM_TEST_IMAGE=TEST_IMAGE, MIGRATION_EXIT="23")
        self.assertEqual(result.returncode, 23, result.stderr)
        calls = self.calls.read_text()
        self.assertEqual(calls.count(" migrate\n"), 1)
        self.assertNotIn(pinned_image(), calls)


if __name__ == "__main__":
    unittest.main()
