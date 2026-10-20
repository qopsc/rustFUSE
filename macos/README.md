# rustFUSE for Mac

A menu bar app that puts rustFUSE volumes in Finder through Apple's File
Provider, the way iCloud Drive works: each volume shows under Locations,
files download when first opened, and macOS can free their space again.
It needs no kernel extension or macFUSE. How it fits together is in
[docs/design.md §2.2](../docs/design.md).

- `App/`: the host app. It adds volumes (`rustfuse attach --lazy` into the
  app group container), registers each as a File Provider domain, keeps a
  `rustfuse sync --provider-socket` running for each, and tells macOS when
  a volume changes. Hub tokens are kept in the login keychain.
- `Extension/`: the File Provider extension. It holds no sync logic and
  asks the volume's daemon for everything over its socket.
- `RustFuseKit/`: what both share: the wire protocol, the socket client and
  the app group layout. `swift test` checks the protocol against the same
  samples the daemon's tests use.

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
(`Contents/Helpers/rustfuse`). CI builds it this way, unsigned.

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
scheme, choose **Volumes…** in the menu bar item and add a volume (its name
in the bucket, the hub address and the hub token). It appears in Finder
under Locations.

## Not checked on a Mac yet

All of this compiles in CI, but none of it has run on a Mac yet. Before
building further, check:

- `git status` on a large tree of files that are not downloaded yet;
- whether reads ever fail with `EDEADLK`;
- whether macOS needs the working set to list every item, which it now
  leaves empty, learning items as folders are browsed;
- how tools that resolve symlinks treat `~/Library/CloudStorage/`.
