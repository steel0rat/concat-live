"""Release manifests keep fork binaries separate from the upstream model mirror."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("concat_release_models", ROOT / "scripts/models.py")
models = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(models)


class ReleaseManifestTests(unittest.TestCase):
    def setUp(self):
        parent = ROOT / "src/target/release-packaging-tests"
        parent.mkdir(parents=True, exist_ok=True)
        self.directory = Path(tempfile.mkdtemp(dir=parent))
        self.bundle = self.directory / "Concat-0.3.0-macos-arm64.dmg"
        self.bundle.write_bytes(b"synthetic release package")

    def test_binary_urls_and_identity_belong_to_concat_live(self):
        manifest = models.release_manifest("0.3.0", "v0.3.0", self.directory)
        package = manifest["binaries"]["macos"]["arm64"]["dmg"]
        self.assertEqual(package["url"],
                         "https://github.com/steel0rat/concat-live/releases/download/v0.3.0/"
                         "Concat-0.3.0-macos-arm64.dmg")
        self.assertEqual(manifest["product"], "Concat Live")
        self.assertEqual(manifest["bundleIdentifier"], "ru.steel0rat.concat-live")
        self.assertEqual(package["bytes"], self.bundle.stat().st_size)
        self.assertEqual(package["sha256"], models.digest(self.bundle))

    def test_missing_targets_are_absent_and_models_keep_the_upstream_mirror(self):
        manifest = models.release_manifest("0.3.0", "v0.3.0", self.directory)
        self.assertEqual(set(manifest["binaries"]), {"macos"})
        self.assertEqual(set(manifest["binaries"]["macos"]), {"arm64"})
        self.assertTrue(manifest["models"])
        for entry in manifest["models"].values():
            self.assertTrue(entry["url"].startswith(
                "https://github.com/jub0t/Concat/releases/download/"))

    def test_mismatched_or_unsafe_release_identifiers_are_refused(self):
        for version, tag in [("0.3.0", "v0.4.0"), ("0.3.0", "../v0.3.0"),
                             ("0.3.0/escape", "v0.3.0/escape")]:
            with self.subTest(version=version, tag=tag), self.assertRaises(ValueError):
                models.release_manifest(version, tag, self.directory)

    def test_an_existing_bundle_must_not_be_a_symlink(self):
        external = self.directory / "original.dmg"
        external.write_bytes(b"outside package selection")
        linked = self.directory / "Concat-0.3.0-macos-x86_64.dmg"
        linked.symlink_to(external)
        with self.assertRaises(ValueError):
            models.release_manifest("0.3.0", "v0.3.0", self.directory)


if __name__ == "__main__":
    unittest.main()
