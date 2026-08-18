import sys
import unittest
from pathlib import Path


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))

from media_tools import _safe_loopback_url  # noqa: E402


class MediaToolsTests(unittest.TestCase):
    def test_accepts_loopback_runtime_urls(self) -> None:
        self.assertEqual(
            _safe_loopback_url("http://127.0.0.1:42421/v1"),
            "http://127.0.0.1:42421",
        )
        self.assertEqual(
            _safe_loopback_url("http://[::1]:42421"),
            "http://[::1]:42421",
        )

    def test_rejects_non_loopback_runtime_urls(self) -> None:
        self.assertIsNone(_safe_loopback_url("https://example.com:42421"))
        self.assertIsNone(_safe_loopback_url("http://127.0.0.1.evil.example:42421"))
        self.assertIsNone(_safe_loopback_url("http://127.0.0.1:42421@evil.example"))
        self.assertIsNone(_safe_loopback_url("http://127.0.0.1:42421/v1?target=evil"))


if __name__ == "__main__":
    unittest.main()
