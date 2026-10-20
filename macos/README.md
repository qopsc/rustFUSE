# rustFUSE for Mac

A menu bar app with a main window (macOS 15 or later) that syncs rustFUSE
volumes on this Mac. Each volume is shown one of three ways:

- **In Finder** through Apple's File Provider, the way iCloud Drive works:
  under Locations, files download when first opened. Needs no kernel
  extension or macFUSE.
- **As a folder** anywhere, holding a full copy.
- **As a mount** anywhere, through macFUSE (FSKit or its kernel
  extension); files download when first opened. Greyed out until macFUSE
  is installed.

Volumes come from saved **connections**: a hub address, its token and CA,
and optionally S3 keys to read the bucket directly. The app lists the
volumes a connection's token opens, adds them here, or creates a new
volume from a folder. Each lazy volume has its own cache size (20 GB by
default); the window shows what each volume takes on disk against it.

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
- `RustFuseKit/`: what both share: the wire protocol, the socket client, the
  app group layout and the saved library (connections, volumes, settings)
  with the daemon command lines built from it. `swift test` checks the
  protocol against the same samples the daemon's tests use.

## Build

Needs Xcode, Rust and [XcodeGen](https://github.com/yonaskolb/XcodeGen)
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
way, unsigned.

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

## Not checked on a Mac yet

All of this compiles in CI, but none of it has run on a Mac yet. Before
building further, check:

- `git status` on a large tree of files that are not downloaded yet;
- whether reads ever fail with `EDEADLK`;
- whether macOS needs the working set to list every item, which it now
  leaves empty, learning items as folders are browsed;
- how tools that resolve symlinks treat `~/Library/CloudStorage/`.
