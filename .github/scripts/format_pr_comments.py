#!/usr/bin/env python3
"""
Format GitHub PR comments, reviews, and review threads for Claude Code Review.

This script takes GitHub GraphQL API response JSON and formats it into
a readable markdown structure for consumption by Claude AI code reviewer.
"""

import json
import sys
from typing import Dict, List, Optional, Any

# The formatted text is passed to the review action as an input, which GitHub
# delivers through an environment variable. Linux caps a single environment
# string at 128 KiB (bytes, not characters), so keep the rendered context
# well under that, leaving room for the surrounding prompt.
MAX_OUTPUT_BYTES = 60_000


def get_author(node: Dict[str, Any]) -> str:
    """
    Safely get author login, handling deleted accounts.

    Args:
        node: GitHub API node containing author information

    Returns:
        Author login string or 'ghost' for deleted accounts
    """
    author = node.get('author')
    return author.get('login', 'ghost') if author else 'ghost'


def truncate_diff_hunk(diff_hunk: str, max_length: int = 500) -> str:
    """
    Truncate diff hunk at line boundaries to avoid cutting mid-line.

    Args:
        diff_hunk: The diff hunk text to truncate
        max_length: Maximum character length before truncation

    Returns:
        Truncated diff hunk with indicator if truncation occurred
    """
    if len(diff_hunk) <= max_length:
        return diff_hunk

    lines = diff_hunk.split('\n')
    truncated_lines = []
    char_count = 0

    for line in lines:
        line_length = len(line) + 1  # +1 for newline
        if char_count + line_length > max_length and truncated_lines:
            break
        truncated_lines.append(line)
        char_count += line_length

    return '\n'.join(truncated_lines) + "\n... (truncated)"


def format_general_comments(pr: Dict[str, Any], output: List[str]) -> None:
    """Format general PR comments section."""
    comments_data = pr.get('comments', {})
    issue_comments = comments_data.get('nodes', [])
    total_comments = comments_data.get('totalCount', 0)

    output.append("## General PR Comments")
    output.append("")

    if issue_comments:
        for comment in issue_comments:
            author = get_author(comment)
            created = comment.get('createdAt', '')[:10]
            body = comment.get('body', '').strip()

            output.append(f"### Comment by @{author} on {created}")
            output.append(body)
            output.append("")
            output.append("-" * 40)
            output.append("")

        if total_comments > 100:
            output.append(f"*Note: This PR has {total_comments} comments. Showing first 100.*")
            output.append("")
    else:
        output.append("No general comments found.")
        output.append("")


def format_review_summaries(pr: Dict[str, Any], output: List[str]) -> None:
    """Format review summaries section."""
    reviews_data = pr.get('reviews', {})
    reviews = reviews_data.get('nodes', [])

    if not reviews:
        return

    output.append("## Review Summaries")
    output.append("")

    for review in reviews:
        if review.get('body'):
            author = get_author(review)
            state = review.get('state', 'COMMENTED')
            created = review.get('createdAt', '')[:10]
            body = review.get('body', '').strip()

            output.append(f"### {state} Review by @{author} on {created}")
            output.append(body)
            output.append("")
            output.append("-" * 40)
            output.append("")


def format_review_thread(thread: Dict[str, Any], output: List[str], show_resolved: bool = False) -> None:
    """
    Format a single review thread with its comments.

    Args:
        thread: Review thread data from GitHub API
        output: Output list to append formatted lines to
        show_resolved: Whether this is a resolved thread (affects formatting)
    """
    path = thread.get('path', 'unknown')
    line = thread.get('line')
    line_str = f"L{line}" if line else "file-level"
    is_outdated = thread.get('isOutdated', False)
    is_resolved = thread.get('isResolved', False)

    # Build header with status indicators
    outdated_marker = " [OUTDATED]" if is_outdated else ""
    status = "RESOLVED" if is_resolved else "UNRESOLVED"

    output.append(f"### Thread: {path}:{line_str}{outdated_marker}")
    output.append(f"**Status:** {status}")
    output.append("")

    # Format comments in thread
    comments = thread.get('comments', {}).get('nodes', [])
    for i, comment in enumerate(comments):
        author = get_author(comment)
        body = comment.get('body', '').strip()
        created = comment.get('createdAt', '')[:10]

        # Show diff hunk for first comment (provides code context)
        if i == 0 and comment.get('diffHunk'):
            diff_hunk = truncate_diff_hunk(comment['diffHunk'])
            output.append("**Code context:**")
            output.append("```")
            output.append(diff_hunk)
            output.append("```")
            output.append("")

        prefix = "Original comment" if i == 0 else f"Reply {i}"
        output.append(f"**{prefix} by @{author} on {created}:**")
        output.append(body)
        output.append("")

    output.append("-" * 40)
    output.append("")


def format_review_threads(pr: Dict[str, Any], output: List[str], resolved: bool) -> None:
    """Format the resolved or the unresolved review threads."""
    review_threads = pr.get('reviewThreads', {}).get('nodes', [])

    label = "Resolved" if resolved else "Unresolved"
    threads = [t for t in review_threads if t.get('isResolved', False) == resolved]

    output.append(f"## {label} Code Review Discussions")
    output.append("")

    if threads:
        for thread in threads:
            format_review_thread(thread, output, show_resolved=resolved)
    else:
        output.append(f"No {label.lower()} discussions.")
        output.append("")


def format_pr_comments(comments_json: str) -> str:
    """
    Main formatting function that orchestrates the output generation.

    Args:
        comments_json: JSON string from GitHub GraphQL API

    Returns:
        Formatted markdown string ready for Claude consumption

    Raises:
        json.JSONDecodeError: If JSON parsing fails
        KeyError: If expected data structure is missing
    """
    try:
        data = json.loads(comments_json)

        # Check for GraphQL errors
        if 'errors' in data:
            error_msg = data['errors'][0].get('message', 'Unknown error')
            return f"⚠️ GitHub API error: {error_msg}"

        # Extract PR data
        pr = data.get('data', {}).get('repository', {}).get('pullRequest')
        if not pr:
            return "⚠️ No PR data found in API response."

    except (json.JSONDecodeError, KeyError) as e:
        return f"⚠️ Unable to parse comment data: {e}"

    # Build formatted output
    output = []
    output.append("=" * 80)
    output.append("EXISTING PR COMMENTS AND DISCUSSIONS")
    output.append("=" * 80)
    output.append("")

    # Highest-signal sections first so truncation drops the least useful text.
    format_review_threads(pr, output, resolved=False)
    total_threads = pr.get('reviewThreads', {}).get('totalCount', 0)
    if total_threads > 100:
        output.append(f"*Note: This PR has {total_threads} review threads. Showing first 100.*")
        output.append("")
    format_review_summaries(pr, output)
    format_general_comments(pr, output)
    format_review_threads(pr, output, resolved=True)

    output.append("=" * 80)
    output.append("END OF EXISTING COMMENTS")
    output.append("=" * 80)

    return truncate_output("\n".join(output))


def truncate_output(text: str, max_bytes: int = MAX_OUTPUT_BYTES) -> str:
    """Cap the rendered context's UTF-8 size at a line boundary, noting the truncation."""
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    head = encoded[:max_bytes].decode("utf-8", errors="ignore")
    cut = head.rfind("\n")
    if cut > 0:
        head = head[:cut]
    return head + "\n\n*Note: existing discussion truncated to fit the review prompt.*"


def main() -> int:
    """
    Main entry point for the script.

    Reads COMMENTS_JSON from environment variable and writes formatted
    output to stdout.

    Returns:
        Exit code: 0 for success, 1 for error
    """
    import os

    comments_json = os.environ.get('COMMENTS_JSON')
    if not comments_json:
        print("⚠️ COMMENTS_JSON environment variable not set.", file=sys.stderr)
        return 1

    try:
        formatted_output = format_pr_comments(comments_json)
        print(formatted_output)
        return 0
    except Exception as e:
        print(f"⚠️ Unexpected error formatting comments: {e}", file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
