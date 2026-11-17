import unittest
import uuid

from scripts.tests.release import load

sbom = load("sbom")

META = {
    "workspace_members": ["sekisho 0.0.0 (path+file:///workspace)"],
    "packages": [
        {"id": "sekisho 0.0.0 (path+file:///workspace)", "name": "sekisho",
         "version": "0.0.0", "license": "Apache-2.0"},
        {"id": "libc 0.2.0 (registry+https://github.com/rust-lang/crates.io-index)", "name": "libc",
         "version": "0.2.0", "license": "MIT/Apache-2.0"},
    ],
}


def with_libc(version: str) -> dict:
    packages = [dict(p) for p in META["packages"]]
    packages[1]["version"] = version
    return {**META, "packages": packages}


class BomFromMetadata(unittest.TestCase):
    def test_names_the_crate_and_lists_dependencies_with_licenses(self):
        doc = sbom.bom_from_metadata(META)
        self.assertEqual(doc["bomFormat"], "CycloneDX")
        self.assertEqual(doc["metadata"]["component"]["purl"], "pkg:cargo/sekisho@0.0.0")
        self.assertEqual(doc["metadata"]["component"]["type"], "application")
        [dep] = doc["components"]
        self.assertEqual(dep["purl"], "pkg:cargo/libc@0.2.0")
        self.assertEqual(dep["licenses"], [{"expression": "MIT OR Apache-2.0"}])

    def test_serial_number_is_stable_and_tracks_the_dependency_set(self):
        # actions/attest-sbom only recognizes CycloneDX with a serialNumber.
        serial = sbom.bom_from_metadata(META)["serialNumber"]
        self.assertEqual(uuid.UUID(serial.removeprefix("urn:uuid:")).version, 5)
        self.assertEqual(sbom.bom_from_metadata(META)["serialNumber"], serial)
        self.assertNotEqual(sbom.bom_from_metadata(with_libc("0.2.1"))["serialNumber"], serial)

    def test_refuses_metadata_without_the_crate(self):
        with self.assertRaisesRegex(SystemExit, "did not include package sekisho"):
            sbom.bom_from_metadata({"workspace_members": [], "packages": META["packages"][1:]})


if __name__ == "__main__":
    unittest.main()
