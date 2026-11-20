import hashlib
import importlib.machinery
import importlib.util
from pathlib import Path
import tempfile
import unittest


loader = importlib.machinery.SourceFileLoader(
    "check_dgx_vendor", str(Path(__file__).resolve().parents[1] / "check-dgx-vendor")
)
spec = importlib.util.spec_from_loader(loader.name, loader)
checker = importlib.util.module_from_spec(spec)
loader.exec_module(checker)


class VendorCheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("core/src/lib.rs", "// Modified by Chia Network for timelord-app\nfn main() {}\n")
        self.write("core/src/added.rs", "// Added by Chia Network for timelord-app\nfn added() {}\n")
        self.write("core/LICENSE", "Apache License\n")
        self.source = {"core/src/lib.rs": self.digest(b"upstream lib\n")}

    def digest(self, data):
        return hashlib.sha256(data).hexdigest()

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def origin(self):
        return {"source_sha256": dict(self.source), "vendor_sha256": checker.hash_tree(self.root)}

    def test_clean_tree_passes(self):
        self.assertEqual(checker.check_tree(self.root, self.origin()), [])

    def test_edited_file_fails(self):
        origin = self.origin()
        self.write("core/src/added.rs", "// Added by Chia Network for timelord-app\nfn changed() {}\n")
        problems = checker.check_tree(self.root, origin)
        self.assertTrue(any("hash mismatch" in problem and "added.rs" in problem for problem in problems))

    def test_extra_file_fails(self):
        origin = self.origin()
        self.write("core/src/extra.rs", "// Added by Chia Network for timelord-app\n")
        problems = checker.check_tree(self.root, origin)
        self.assertTrue(any("extra" in problem and "extra.rs" in problem for problem in problems))

    def test_missing_notice_on_modified_file_fails(self):
        self.write("core/src/lib.rs", "// no provenance notice\nfn main() {}\n")
        problems = checker.check_tree(self.root, self.origin())
        self.assertTrue(any("change notice" in problem and "lib.rs" in problem for problem in problems))


if __name__ == "__main__":
    unittest.main()
