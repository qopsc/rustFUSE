# rustFUSE for Mac

A menu bar app with a main window (macOS 15 or later) that syncs rustFUSE
volumes on this Mac. Each volume is shown one of three ways:

- **In Finder** through Apple's File Provider, the way iCloud Drive works:
  under Locations, files download when first opened. Needs no kernel
  extension or macFUSE.
- **As a folder** anywhere, holding a full copy.
- **As a mount** anywhere, through macFUSE (FSKit or its kernel
  extension) or, on macOS 26, rustFUSE's own file system extension (no
  macFUSE; turn it on once in System Settings › General › Login Items &
  Extensions › File System Extensions); files download when first opened.
  Greyed out until the chosen backend is available.

Volumes come from saved **connections**: a hub address, its token and CA,
and optionally S3 keys to read the bucket directly. The app lists the
volumes a connection's token opens, adds them here, or creates a new
volume from a folder. Each lazy volume has its own cache size (20 GB by
default); the window shows what each volume takes on disk against it.

**Browsing.** Each volume's page has three views: an overview (disk use,
cache size, and conflicts: conflict copies to reveal in Finder, and
branches that diverged, with the `git merge` to run), its **files** on this
Mac (downloaded, kept downloaded, only in the bucket or only on this Mac,
with Keep Downloaded and Free Up Space for volumes that download on open),
and the volume **in the bucket**. The bucket view, also reached with
Browse next to any volume of a connection, reads the volume without
syncing it (`rustfuse ls`) and downloads files and folders anywhere
(`rustfuse get`).

**Settings:** what quitting does (stop syncing, or keep syncing in the
background through a login agent per volume), open at login, the default
cache size, and the mount backend.

How it fits together is in [docs/design.md §2.2](../docs/design.md).

- `App/`: the host app. It adds volumes (`rustfuse attach`, or `init` for a
  new one), registers Finder volumes as File Provider domains, keeps a
  daemon running for each volume (`rustfuse sync --provider-socket`, or
  `rustfuse mount` for a mount), and tells macOS when a Finder volume
  changes. Secrets are kept in the login keychain. With "keep syncing", a
  launchd login agent per volume runs the app headless
  (`rustFUSE --daemon <volume>`) instead.
- `Extension/`: the File Provider extension. It holds no sync logic and
  asks the volume's daemon for everything over its socket.
- `FS/`: the FSKit module behind `rustfuse mount --native` (macOS 26). It
  serves the synced directory as a passthrough and asks the daemon to fill
  placeholders in before they are read. Its target builds in Swift 5 mode,
  since FSKit's protocols predate strict concurrency.
- `RustFuseKit/`: what both share: the wire protocol, the socket client, the
  app group layout and the saved library (connections, volumes, settings)
  with the daemon command lines built from it. `swift test` checks the
  protocol, `rustfuse ls --json` and `rustfuse conflicts --json` against
  the same samples the daemon's tests use, and `library.json` against the
  sample `rustfuse tui` reads.

`rustfuse tui` shares the library and keychain items with the app. A folder
or mount volume it started runs as its own launchd agent; the app then
shows it as running from the terminal UI and leaves it alone.

## Build

Needs Xcode 26 or later (the code is Swift 6, and uses Liquid Glass on
macOS 26 and later while still running on macOS 15), Rust and [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`).

```sh
cd macos
xcodegen generate
xcodebuild -project rustFUSE.xcodeproj -scheme rustFUSE -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO build
```

The build compiles `rustfuse` with cargo and puts it inside the app
(`Contents/Helpers/rustfuse`). Where macFUSE is installed (`brew install
--cask macfuse pkgconf`), it also bundles a build with macFUSE support
(`Contents/Helpers/rustfuse-macfuse`) for mount mode. CI builds it this
way, unsigned, then runs `rustFUSE --screenshots <dir>`, which shows the
window, its views and sheets with sample data and saves a PNG of each (the
CI run's `screenshots` artifact).

## Run

macOS only loads a File Provider extension when the app and extension are
signed by one team and share an app group. Xcode's free personal team may
be enough to try it locally. Distributing it needs a paid Apple Developer
account (Developer ID signing and notarization). Put your team in
`Config/Local.xcconfig` (git ignores it):

```
DEVELOPMENT_TEAM = ABCDE12345
RUSTFUSE_BUNDLE_PREFIX = com.example.rustfuse
```

Then run `xcodegen generate`, open `rustFUSE.xcodeproj`, run the `rustFUSE`
scheme, choose **Open rustFUSE…** in the menu bar item, add a connection
(the hub address and token), then add one of its volumes.

## Signing and releasing

A release tag (see the main README) also builds the app with Developer ID,
notarizes it and attaches `rustFUSE-<version>.zip` to the release; the Mac
command line tools in the release are signed and notarized too. This runs
only when the repository has these secrets (Settings › Secrets and variables
› Actions); without them, releases carry unsigned tools and no app.
`macos/sign.sh` does the work.

One-time setup in a paid Apple Developer account:

1. **Certificate.** In Certificates, create a *Developer ID Application*
   certificate, install it, and export it with its key from Keychain
   Access as a `.p12` with a password.
2. **Identifiers.** Register three App IDs: `<prefix>.app`,
   `<prefix>.app.fileprovider` and `<prefix>.app.fs`, where `<prefix>` is
   `dev.rustfuse` unless you set `APPLE_BUNDLE_PREFIX`. Give all three the
   *App Groups* capability (the app uses the group `<TEAMID>.<prefix>`;
   team-prefixed groups on macOS need no group registered), and
   `<prefix>.app.fs` also the *FSKit Module* capability.
3. **Profiles.** Create a *Developer ID* provisioning profile for each App
   ID and download them.
4. **Notary key.** In App Store Connect › Users and Access › Integrations,
   create an API key with the Developer role; note its key ID and issuer
   ID, and download the `.p8`.
5. **Secrets.** Add them to the repository (`base64 -i <file>` for files):

| Secret | Value |
| --- | --- |
| `APPLE_TEAM_ID` | the team ID, e.g. `ABCDE12345` |
| `APPLE_BUNDLE_PREFIX` | optional, default `dev.rustfuse` |
| `APPLE_DEVELOPER_ID_P12` | base64 of the `.p12` |
| `APPLE_DEVELOPER_ID_P12_PASSWORD` | its password |
| `APPLE_PROFILE_APP` | base64 of the app's profile |
| `APPLE_PROFILE_FILEPROVIDER` | base64 of the File Provider extension's profile |
| `APPLE_PROFILE_FS` | base64 of the FSKit module's profile |
| `APPLE_API_KEY_ID` | the API key's ID |
| `APPLE_API_ISSUER_ID` | its issuer ID |
| `APPLE_API_KEY_P8` | base64 of the `.p8` |

The release workflow runs on pull requests that change `macos/sign.sh`, so
a signing change can be tried before a tag.

## Not checked on a Mac yet

All of this compiles in CI, but none of it has run on a Mac yet. Before
building further, check:

- `git status` on a large tree of files that are not downloaded yet;
- whether reads ever fail with `EDEADLK`;
- whether macOS needs the working set to list every item, which it now
  leaves empty, learning items as folders are browsed;
- how tools that resolve symlinks treat `~/Library/CloudStorage/`.
