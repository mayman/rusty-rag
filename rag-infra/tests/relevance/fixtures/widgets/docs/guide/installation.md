# Installation

Download the release archive for your platform and unpack it somewhere on your PATH.

## Requirements

You need git 2.30 or newer on the PATH.

## Windows path length

Windows limits paths to 260 characters unless long paths are enabled. Set
`core.longpaths` in git and turn on the long path setting before you add a
repository with deep directories.

## Troubleshooting

If the build fails, run `cargo clean` and try again.
