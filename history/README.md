# history

The record of how this repository's history was extracted from the monorepo,
[The Test Cabinet](https://github.com/TheClockwyrks/TheTestCabinet), where the
publisher crate lived until the extraction. Nothing builds or tests against
these files; they are kept so the provenance outlives the scratch clone the
extraction ran in.

## The extraction

The history below the scaffold commit is the monorepo's `issue/repos-15` at
`24a546b594afce881038dcd80086e05d63f6c57c` (its `nightly`; no other crate of
the monorepo depended on this one, so nothing had to be carried out of it
first), filtered to the paths in [`paths.txt`](paths.txt) by git filter-repo
2.47.0:

```sh
git clone -q --no-local --single-branch --branch issue/repos-15 --no-tags \
  <the-test-cabinet> x
cd x
uvx --from git-filter-repo==2.47.0 git-filter-repo --force \
  --paths-from-file ../paths.txt --prune-empty always --preserve-commit-hashes
```

Of the 7,173 commits reachable from that commit, 103 touched a path in the
list and were kept (18 of them merges); the rest were pruned. No tag was
carried. The tip tree of `crates/publisher/` hashes the same as the monorepo's
at that commit:

```text
crates/publisher   3a165280d8d8650d1c4427e4c97c7aa0b92d24d6
```

The paths are kept where they were in the monorepo, so the filter needed no
rename.

## The files

- [`paths.txt`](paths.txt) is the list handed to `--paths-from-file`
  (sha256 `a7edec65ca203399fe55a8b6ca3b9627143aceea11f80de07aa9d7aab57f11b2`):
  the crate's directory, `crates/publisher/`, and every path a tracked file
  under it had before a rename or copy, collected by
  `git log --follow --name-status -M -C` over each of the 21 files, so a
  file's history reaches back past the commits that moved it into place.
- [`commit-map`](commit-map) is filter-repo's map from each monorepo commit
  (left) to the commit it became here (right), all zeros for a pruned one.
  `--preserve-commit-hashes` left the commit ids quoted in the messages as they
  were, so an id in a message below the scaffold commit names a monorepo
  commit, and this map translates it.

## The live-path review

`paths.txt` names two paths outside the moved set, each the path a file here
had before it was moved into place:

- `crates/core/src/publish.rs` and `crates/core/src/publish.test.rs`: the
  former core crate's publish module and its tests, which
  `src/release.rs` and `src/release.test.rs` began as.

Neither is live in the monorepo's tree at the extraction commit (the core
crate had been folded into the driver and deleted), so the filtered tip
carries no file outside `crates/publisher/` and the scaffold commit deletes
nothing.
