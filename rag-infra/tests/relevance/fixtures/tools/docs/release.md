# Release process

## Signing keys

Release archives are signed with the team key. Never commit the private key; keep it in
the vault and load it only on the release machine.

## Publishing

Tag the commit, push the tag, and attach the signed archives to the release.
