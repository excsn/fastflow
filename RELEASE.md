# Releasing fastflow

A release is a notarized Apple Silicon `.dmg` on a GitHub release of `excsn/fastflow` plus a cask in `excsn/homebrew-tap` that points at it. Users install and upgrade with:

```sh
brew install --cask excsn/tap/fastflow
brew upgrade --cask fastflow
```

## Contents

* [One-time setup](#one-time-setup)
* [Cutting a release](#cutting-a-release)
* [What the release script does](#what-the-release-script-does)
* [The cask](#the-cask)
* [When something fails](#when-something-fails)

## One-time setup

The release Mac needs the following.

- **Rust target.** `aarch64-apple-darwin`, which is the host target on Apple Silicon.
- **Developer ID Application certificate.** It is created in Xcode under Settings → Accounts → the team → Manage Certificates → + → Developer ID Application. Only the team's Account Holder or an Admin can create one. Check it with:

  ```sh
  security find-identity -v -p codesigning
  ```

  The name it prints, `Developer ID Application: <team name> (<team id>)`, is what `FASTFLOW_RELEASE_IDENTITY` takes.
- **Notarization credentials** in the keychain under the profile name `fastflow`. Create an app-specific password at account.apple.com under Sign-In and Security → App-Specific Passwords, then run this and paste the password when asked:

  ```sh
  xcrun notarytool store-credentials fastflow --apple-id <apple id> --team-id <team id>
  ```

  It ends with "Credentials saved to Keychain". The password is never needed again.
- **ImageMagick**, only to regenerate the icons with `scripts/icons.sh`.

## Cutting a release

1. Set the version in the root `Cargo.toml` under `[workspace.package]` and commit it.
2. Build, sign and notarize:

   ```sh
   FASTFLOW_RELEASE_IDENTITY="Developer ID Application: <team name> (<team id>)" scripts/release.sh
   ```

   Notarization usually takes two to five minutes. The script prints the dmg path, its sha256 and the cask path:

   ```
   dmg   dist/fastflow-<version>.dmg
   sha   <sha256>
   cask  dist/fastflow.rb
   ```

3. Publish a GitHub release at github.com/excsn/fastflow/releases/new with tag `v<version>` on `main`, title `fastflow <version>` and `dist/fastflow-<version>.dmg` attached. The cask's download URL depends on the tag and file name matching exactly.
4. Check the download matches the cask:

   ```sh
   curl -sL https://github.com/excsn/fastflow/releases/download/v<version>/fastflow-<version>.dmg | shasum -a 256
   ```

5. Update the tap:

   ```sh
   git clone git@github.com:excsn/homebrew-tap.git
   cp dist/fastflow.rb homebrew-tap/Casks/fastflow.rb
   git -C homebrew-tap commit -am "fastflow <version>"
   git -C homebrew-tap push
   ```

6. Install on a Mac that does not build fastflow and launch it:

   ```sh
   brew update && brew upgrade --cask fastflow    # or brew install --cask excsn/tap/fastflow
   ```

`scripts/release.sh --no-notarize` runs everything except notarization. Use it to check a build and its signatures without uploading to Apple. Gatekeeper rejects its dmg on other Macs.

## What the release script does

| step | how |
|---|---|
| build | `cargo build --release --target aarch64-apple-darwin` for `fastflow_ui_macos` and `fastflow_cli` |
| assemble | `fastflow_ui_macos/bundle/assemble.sh` puts `fastflow-app` and the `fastflow` CLI in `fastflow.app/Contents/MacOS` with the icon and Info.plist |
| sign | the CLI first and then the app, both with `--options runtime --timestamp`, which notarization requires |
| package | `hdiutil` makes a compressed dmg holding the app and an `Applications` link, then the dmg is signed |
| notarize | `xcrun notarytool submit --keychain-profile fastflow --wait`, then `xcrun stapler staple` on the dmg |
| verify | `spctl` must report `source=Notarized Developer ID` for the dmg and the app |
| cask | `packaging/fastflow.rb` with `@VERSION@` and `@SHA256@` filled in, written to `dist/fastflow.rb` |

`dist/` is ignored by git. `FASTFLOW_NOTARY_PROFILE` overrides the keychain profile name.

The build is Apple Silicon only. The cask declares `depends_on arch: :arm64`, so Homebrew refuses on an Intel Mac instead of installing an app that cannot launch.

## The cask

`packaging/fastflow.rb` is the template. The copy in `excsn/homebrew-tap` is generated from it and never edited by hand.

| stanza | effect |
|---|---|
| `depends_on formula: "ffmpeg"` and `"webp"` | installs the render and WebP tools first |
| `app "fastflow.app"` | copies the app to `/Applications` |
| `binary ".../Contents/MacOS/fastflow"` | links the CLI onto the PATH |
| `uninstall quit:` | quits the running app before removing it |
| `zap trash:` | `brew uninstall --zap` also removes settings and logs. Recordings in `~/Movies/fastflow` are kept |

A change to the template reaches users at the next release. To ship it sooner, copy a regenerated cask with the same version and sha256 to the tap.

## When something fails

- **Notarization returns Invalid.** Fetch Apple's log with the submission id the script printed:

  ```sh
  xcrun notarytool log <submission id> --keychain-profile fastflow
  ```

  The usual causes are a binary signed without the hardened runtime or a secure timestamp.
- **`No Keychain password item found for profile: fastflow`.** The notarization credentials are missing. Run the `store-credentials` step above.
- **`FASTFLOW_RELEASE_IDENTITY` is not set.** The script refuses to run without a Developer ID identity.
- **`brew` reports a sha256 mismatch.** The dmg on the release is not the one the cask was generated from. Upload `dist/fastflow-<version>.dmg` again or rerun the release and update both.
- **The app opens on the build Mac but not on another.** Check `spctl -a -vv -t exec /Applications/fastflow.app` there. Anything other than `Notarized Developer ID` means the dmg was built with `--no-notarize`.
