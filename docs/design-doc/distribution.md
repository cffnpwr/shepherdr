# ビルドと配布

## タスクランナー

ビルド・開発タスクはmise tasksで定義する。
ツールバージョンの固定（Rust・bun）とタスク定義を`mise.toml`に集約する。

## ビルドと署名

- appバンドルは`cargo tauri build`で生成する。
- 署名はad-hoc（識別子`com.cffnpwr.shepherdr`）とする。署名証明書は使わない。
- ad-hoc署名ではLNP承認がcdhash単位のため、バイナリが変わるビルドごとに承認がリセットされる（[全体アーキテクチャ](./architecture.md)）。再承認の運用は[運用](./operations.md)で定める。

## リリースと配布

- GitHub Actionsでタグからリリースビルドを生成し、GitHub Releasesに添付する。
- 配布は`cffnpwr/homebrew-tap`のcaskで行う。
- ダウンロード取得物にはquarantine属性が付き、ad-hoc署名のappはそのままではGatekeeperに拒否されるため、quarantine解除の手順（`--no-quarantine`等）の案内をtap側で持つ。

## 開発時フロー

- 通常の開発は`cargo tauri dev`で行う。
- LNP承認は`/Applications`に配置したappバンドルに対して成立するため、LANアクセスを伴う実サービス込みのE2E確認はインストール済みビルドで行う。
