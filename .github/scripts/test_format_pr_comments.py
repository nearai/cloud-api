#!/usr/bin/env python3
"""
Unit tests for format_pr_comments.py

Run with: python3 -m pytest test_format_pr_comments.py
Or simply: python3 test_format_pr_comments.py
"""

import unittest
import json
from format_pr_comments import (
    get_author,
    truncate_diff_hunk,
    format_pr_comments
)


class TestGetAuthor(unittest.TestCase):
    """Test author extraction with edge cases."""

    def test_normal_author(self):
        """Test normal author with login."""
        node = {'author': {'login': 'johndoe'}}
        self.assertEqual(get_author(node), 'johndoe')

    def test_deleted_account(self):
        """Test deleted account (author is None)."""
        node = {'author': None}
        self.assertEqual(get_author(node), 'ghost')

    def test_missing_author_field(self):
        """Test missing author field entirely."""
        node = {}
        self.assertEqual(get_author(node), 'ghost')

    def test_author_without_login(self):
        """Test author object without login field."""
        node = {'author': {}}
        self.assertEqual(get_author(node), 'ghost')


class TestTruncateDiffHunk(unittest.TestCase):
    """Test diff hunk truncation logic."""

    def test_no_truncation_needed(self):
        """Test diff hunk shorter than max length."""
        diff = "line 1\nline 2\nline 3"
        result = truncate_diff_hunk(diff, max_length=100)
        self.assertEqual(result, diff)

    def test_truncation_at_line_boundary(self):
        """Test truncation happens at line boundaries."""
        diff = "line 1\nline 2\nline 3\nline 4\nline 5"
        result = truncate_diff_hunk(diff, max_length=20)
        self.assertIn("line 1", result)
        self.assertIn("(truncated)", result)
        self.assertNotIn("line 5", result)

    def test_empty_diff(self):
        """Test empty diff hunk."""
        result = truncate_diff_hunk("", max_length=100)
        self.assertEqual(result, "")

    def test_single_long_line(self):
        """Test single line longer than max length."""
        diff = "a" * 1000
        result = truncate_diff_hunk(diff, max_length=500)
        self.assertIn("(truncated)", result)


class TestFormatPRComments(unittest.TestCase):
    """Test main formatting function."""

    def test_empty_pr(self):
        """Test PR with no comments or reviews."""
        data = {
            'data': {
                'repository': {
                    'pullRequest': {
                        'comments': {'nodes': [], 'totalCount': 0},
                        'reviews': {'nodes': []},
                        'reviewThreads': {'nodes': [], 'totalCount': 0}
                    }
                }
            }
        }
        result = format_pr_comments(json.dumps(data))
        self.assertIn("EXISTING PR COMMENTS", result)
        self.assertIn("No general comments found", result)
        self.assertIn("No unresolved discussions", result)

    def test_graphql_error(self):
        """Test handling of GraphQL errors."""
        data = {
            'errors': [{'message': 'Rate limit exceeded'}]
        }
        result = format_pr_comments(json.dumps(data))
        self.assertIn("GitHub API error", result)
        self.assertIn("Rate limit exceeded", result)

    def test_missing_pr_data(self):
        """Test handling of missing PR data."""
        data = {'data': {}}
        result = format_pr_comments(json.dumps(data))
        self.assertIn("No PR data found", result)

    def test_invalid_json(self):
        """Test handling of invalid JSON."""
        result = format_pr_comments("not valid json")
        self.assertIn("Unable to parse", result)

    def test_pr_with_general_comment(self):
        """Test PR with a general comment."""
        data = {
            'data': {
                'repository': {
                    'pullRequest': {
                        'comments': {
                            'nodes': [{
                                'author': {'login': 'reviewer1'},
                                'body': 'Looks good!',
                                'createdAt': '2025-01-15T12:00:00Z'
                            }],
                            'totalCount': 1
                        },
                        'reviews': {'nodes': []},
                        'reviewThreads': {'nodes': [], 'totalCount': 0}
                    }
                }
            }
        }
        result = format_pr_comments(json.dumps(data))
        self.assertIn("@reviewer1", result)
        self.assertIn("Looks good!", result)
        self.assertIn("2025-01-15", result)

    def test_pr_with_unresolved_thread(self):
        """Test PR with unresolved review thread."""
        data = {
            'data': {
                'repository': {
                    'pullRequest': {
                        'comments': {'nodes': [], 'totalCount': 0},
                        'reviews': {'nodes': []},
                        'reviewThreads': {
                            'nodes': [{
                                'isResolved': False,
                                'isOutdated': False,
                                'path': 'src/main.rs',
                                'line': 42,
                                'comments': {
                                    'nodes': [{
                                        'author': {'login': 'reviewer1'},
                                        'body': 'Consider using Option here',
                                        'createdAt': '2025-01-15T12:00:00Z',
                                        'diffHunk': '@@ -40,3 +40,3 @@\n let x = 5;'
                                    }]
                                }
                            }],
                            'totalCount': 1
                        }
                    }
                }
            }
        }
        result = format_pr_comments(json.dumps(data))
        self.assertIn("Unresolved Code Review Discussions", result)
        self.assertIn("src/main.rs:L42", result)
        self.assertIn("Consider using Option here", result)
        self.assertIn("UNRESOLVED", result)

    def test_pr_with_resolved_thread(self):
        """Test PR with resolved review thread."""
        data = {
            'data': {
                'repository': {
                    'pullRequest': {
                        'comments': {'nodes': [], 'totalCount': 0},
                        'reviews': {'nodes': []},
                        'reviewThreads': {
                            'nodes': [{
                                'isResolved': True,
                                'isOutdated': False,
                                'path': 'src/lib.rs',
                                'line': 10,
                                'comments': {
                                    'nodes': [{
                                        'author': {'login': 'reviewer1'},
                                        'body': 'Fixed',
                                        'createdAt': '2025-01-15T12:00:00Z'
                                    }]
                                }
                            }],
                            'totalCount': 1
                        }
                    }
                }
            }
        }
        result = format_pr_comments(json.dumps(data))
        self.assertIn("Resolved Code Review Discussions", result)
        self.assertIn("src/lib.rs:L10", result)
        self.assertIn("RESOLVED", result)


if __name__ == '__main__':
    # Run tests
    unittest.main()
