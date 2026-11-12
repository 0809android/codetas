import json
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))

import media_tools  # noqa: E402
from media_tools import (  # noqa: E402
    _reject_referencing_video_format,
    _safe_loopback_url,
    _video_duration,
)


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


class VideoProbeTests(unittest.TestCase):
    def test_plain_containers_are_analyzed(self) -> None:
        _reject_referencing_video_format("mov,mp4,m4a,3gp,3g2,mj2")
        _reject_referencing_video_format("matroska,webm")

    def test_containers_that_list_other_inputs_are_refused(self) -> None:
        for format_name in ("hls", "applehttp", "concat", "image2", "lavfi", ""):
            with self.assertRaises(RuntimeError, msg=format_name):
                _reject_referencing_video_format(format_name)

    def _probe(self, stdout: str) -> tuple[list[str], float | None]:
        completed = subprocess.CompletedProcess([], 0, stdout=stdout, stderr="")
        with mock.patch.object(media_tools.shutil, "which", return_value="/usr/bin/ffprobe"), \
                mock.patch.object(media_tools.subprocess, "run", return_value=completed) as run:
            try:
                duration = _video_duration(Path("/videos/clip.mp4"))
            except RuntimeError:
                duration = None
        return run.call_args.args[0], duration

    def test_probe_is_limited_to_the_local_file_and_reads_the_duration(self) -> None:
        stdout = json.dumps({"format": {"format_name": "mov,mp4", "duration": "12.5"}})
        arguments, duration = self._probe(stdout)
        self.assertEqual(duration, 12.5)
        index = arguments.index("-protocol_whitelist")
        self.assertEqual(arguments[index + 1], "file")

    def test_playlist_disguised_as_a_video_is_refused(self) -> None:
        stdout = json.dumps({"format": {"format_name": "hls", "duration": "30"}})
        _, duration = self._probe(stdout)
        self.assertIsNone(duration)


if __name__ == "__main__":
    unittest.main()
