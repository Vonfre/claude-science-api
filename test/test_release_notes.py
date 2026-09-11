"""Render the actual packaging heredoc without macOS tools or application launch."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseNotesTests(unittest.TestCase):
    def test_release_notes_render_with_explicit_variable_boundaries(self):
        source = (ROOT / "scripts/package-sciport-release.sh").read_text()
        marker = 'cat > "$OUTPUT/RELEASE_NOTES.md" <<EOF\n'
        body = source.split(marker, 1)[1].split('\nEOF\n', 1)[0]
        # Linux Bash may accept adjacent UTF-8 punctuation differently from
        # macOS Bash. Assert the portable form as well as executing the text.
        self.assertNotIn('$RELEASE_TAG', body)
        self.assertIn('${RELEASE_TAG}：', body)
        for locale in ('C', 'C.UTF-8'):
            with self.subTest(locale=locale), tempfile.TemporaryDirectory() as tmp:
                env = {
                    'PATH': os.defpath, 'HOME': tmp, 'LC_ALL': locale,
                    'OUTPUT': tmp, 'RELEASE_TAG': 'v0.9.0',
                    'BUILD_SHA': 'e' * 40, 'DMG': 'SciPort_0.9.0_aarch64.dmg',
                }
                result = subprocess.run(
                    ['bash', '--noprofile', '--norc', '-eu', '-o', 'pipefail'],
                    input=marker + body + '\nEOF\n', text=True,
                    env=env, capture_output=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                rendered = (Path(tmp) / 'RELEASE_NOTES.md').read_text()
                self.assertIn('SciPort v0.9.0：', rendered)
                self.assertIn('/blob/v0.9.0/README.md', rendered)
                self.assertIn('SciPort_0.9.0_aarch64.dmg', rendered)
                self.assertIn('`' + 'e' * 40 + '`', rendered)
                self.assertNotIn('${', rendered)
                self.assertIn('not Apple Developer ID signed or notarized', rendered)


if __name__ == '__main__':
    unittest.main()
