"""The cloud-platform schema image is pinned once, in scripts/cloud-platform-schema.env.

docker-compose.yml repeats the pin as its interpolation default, because compose cannot read
the env file. This test fails when the two drift apart.
"""

import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


def pinned_image():
    text = (ROOT / "scripts" / "cloud-platform-schema.env").read_text(encoding="utf-8")
    match = re.search(r"^CLOUD_PLATFORM_IMAGE=(\S+)$", text, re.MULTILINE)
    if match is None:
        raise AssertionError("CLOUD_PLATFORM_IMAGE is not set in cloud-platform-schema.env")
    return match.group(1)


class SchemaPinTest(unittest.TestCase):
    def test_compose_default_matches_pin(self):
        compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
        match = re.search(r"image: \$\{CLOUD_PLATFORM_IMAGE:-([^}]+)\}", compose)
        self.assertIsNotNone(match, "docker-compose.yml has no CLOUD_PLATFORM_IMAGE default")
        self.assertEqual(match.group(1), pinned_image())

    def test_pin_is_a_tagged_or_digest_reference(self):
        self.assertRegex(pinned_image(), r"^ghcr\.io/nearai/cloud-platform[:@]\S+$")


if __name__ == "__main__":
    unittest.main()
