"""render-product.sh renders the Kubernetes product template for one image digest."""

import os
import pathlib
import subprocess
import tempfile
import tomllib
import unittest

from scripts.tests.release import RELEASE

SCRIPT = RELEASE / "render-product.sh"
DIGEST = "sha256:" + "0" * 64
IMAGE = f"ghcr.io/example/sekisho@{DIGEST}"


class RenderProduct(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"rp{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        self.out = pathlib.Path(tmp.name) / "product"

    def render(self, image: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SCRIPT), "--image", image, "--version", "9.8.7", "--out", str(self.out)],
            capture_output=True, text=True,
        )

    def test_pins_the_image_and_the_component_to_the_digest(self):
        result = self.render(IMAGE)
        self.assertEqual(result.returncode, 0, result.stderr)
        product = tomllib.loads((self.out / "tenkai.toml").read_text())
        self.assertEqual(product["product"]["version"], "9.8.7")
        component = product["compatibility"]["components"]["sekisho"]
        self.assertEqual(component["pin"], {"kind": "digest", "value": DIGEST})
        self.assertIn("sekisho.mcp", component["provides"])
        self.assertIn(f'image: "{IMAGE}"', (self.out / "manifests" / "10-statefulset.yaml").read_text())

    def test_refuses_an_image_tag(self):
        result = self.render("ghcr.io/example/sekisho:9.8.7")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.out.exists())


if __name__ == "__main__":
    unittest.main()
