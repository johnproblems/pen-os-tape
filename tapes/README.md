# tapes/

Recorded tapes live here. `os-tape record` writes here by default and
`os-tape check tapes/` (the CI entry point) replays every `*.tape` in this
directory in diff mode.

Tapes are generated artifacts and are git-ignored; commit one only if you
mean to pin it as a regression fixture.
