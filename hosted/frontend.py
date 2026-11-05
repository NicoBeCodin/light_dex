"""Immutable frontend assets; publish index.html only after its assets exist."""
import hashlib
from pathlib import Path


def frontend_files(directory: Path):
    assets = {p.name: p.read_bytes() for p in sorted(directory.iterdir())
              if p.is_file() and p.suffix in {'.js', '.mjs', '.css', '.wasm'}}
    digest = hashlib.sha256()
    for name, data in assets.items():
        digest.update(name.encode() + b'\0' + data)
    release = digest.hexdigest()[:16]
    prefix = f'assets/{release}'
    index = (directory / 'index.html').read_text()
    index = index.replace('/app.css', f'/{prefix}/app.css').replace('/app.js', f'/{prefix}/app.js')
    files = {f'public/{prefix}/{name}': data for name, data in assets.items()}
    files['public/index.html'] = index.encode()
    return release, files
