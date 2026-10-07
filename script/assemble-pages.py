#!/usr/bin/env python3
"""Combine the built public website with existing update feeds without changing them."""
import argparse
import json
from pathlib import Path
import shutil


def assemble(website: Path, output: Path) -> None:
    website, output = website.resolve(), output.resolve()
    if website == output or website in output.parents or output in website.parents:
        raise ValueError('Website and output directories must be separate')
    if not (website / 'index.html').is_file():
        raise ValueError('Built website must contain index.html')
    feeds = {}
    for name in ('updates.json', 'updates-dev.json'):
        path = output / name
        if path.exists():
            content = path.read_bytes()
            data = json.loads(content)
            if not isinstance(data, dict) or not isinstance(data.get('releases'), list):
                raise ValueError(f'Invalid feed: {name}')
            feeds[name] = content
    if 'updates.json' not in feeds:
        raise ValueError('Stable update feed must exist before assembling Pages')
    files = list(website.rglob('*'))
    for source in files:
        relative = source.relative_to(website)
        destination = output / relative
        if destination.is_symlink() or any(parent.is_symlink() for parent in destination.parents if parent != output and output in parent.parents):
            raise ValueError(f'Output asset path contains a symlink: {relative}')
        if source.is_symlink():
            raise ValueError(f'Symlink is not a public website asset: {relative}')
        if relative.parts[0] in ('updates.json', 'updates-dev.json', '.git', '.agents'):
            raise ValueError(f'Website cannot overwrite reserved path: {relative}')
    for source in files:
        destination = output / source.relative_to(website)
        if source.is_dir():
            destination.mkdir(parents=True, exist_ok=True)
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, destination)
    for name, content in feeds.items():
        if (output / name).read_bytes() != content:
            raise RuntimeError(f'Update feed changed during assembly: {name}')
    (output / '.nojekyll').touch()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--website', type=Path, default=Path('website/dist'))
    parser.add_argument('--output', type=Path, default=Path('update-site'))
    options = parser.parse_args()
    assemble(options.website, options.output)
