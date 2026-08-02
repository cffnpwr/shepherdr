# 0xProto (vendored)

`0xProto-Regular.woff2` is not distributed via npm or Fontsource (checked
against `api.fontsource.org/v1/fonts`), so it is vendored directly instead of
being a `package.json` dependency.

- Version: `2.502`
- Source: <https://github.com/0xType/0xProto/releases/tag/2.502>, asset
  `0xProto_2_502.zip`
- File origin in the archive: `fonts/0xProto-Regular.woff2` (ligatures
  enabled, Regular weight, woff2 format)
- License: OFL-1.1. Full license text is bundled at
  `frontend/public/licenses/0xproto/LICENSE` (copied verbatim from the
  release archive's `LICENSE` file).

## Updating

1. `gh release download <new-tag> --repo 0xType/0xProto`
2. Replace `0xProto-Regular.woff2` in this directory with
   `fonts/0xProto-Regular.woff2` from the new archive.
3. Replace `frontend/public/licenses/0xproto/LICENSE` with the new archive's
   `LICENSE` file.
4. Update the version/source above.
