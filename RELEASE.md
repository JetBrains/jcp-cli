## Release

1. [optionally] update dependencies with `cargo update`
2. run `dist init` (cargo-dist) to make sure `ci.yml` workflow is up to date
3. update version in `Cargo.toml`
4. push changes to main branch (via separate PR in GitHub)
2. create a tag with the same version (eg. v0.3.5)
3. push tag to remote

From this point `ci.yaml` GHA workflow will kickin and it will:
   - build release distribution on all platforms
   - create GitHub Release
   - update Homebrew Folmulae for macOS
