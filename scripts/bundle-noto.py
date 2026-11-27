"""Install the pinned PDF fonts and OFL notices beside the packaged application."""

import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path
from urllib.request import urlopen


def bundle(destination):
    assets = json.loads(Path(__file__).with_name("noto-fonts.json").read_text())
    source = os.environ.get("PRIVATE_TRANSLATE_PDF_NOTO_DIR")
    destination.mkdir(parents=True, exist_ok=True)
    for asset in assets:
        name = asset["file"]
        output = destination / name
        if output.is_file() and hashlib.sha256(output.read_bytes()).hexdigest() == asset["sha256"]:
            continue
        if source:
            data = (Path(source) / name).read_bytes()
        else:
            with urlopen(asset["url"], timeout=90) as response:
                data = response.read()
        if hashlib.sha256(data).hexdigest() != asset["sha256"]:
            raise ValueError(f"SHA-256 mismatch for {name}")
        with tempfile.NamedTemporaryFile(dir=destination, delete=False) as temp:
            path = Path(temp.name)
            try:
                temp.write(data)
                temp.close()
                path.chmod(0o644)
                path.replace(output)
            finally:
                path.unlink(missing_ok=True)


if __name__ == "__main__":
    bundle(Path(sys.argv[1]))
