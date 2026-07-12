# ビルドと配布

## タスクランナー

ビルド・開発タスクはmise tasksで定義する。
ツールバージョンの固定（Rust・bun）とタスク定義を`mise.toml`に集約する。

## ビルドと署名

- appバンドルは`cargo tauri build`で生成する。
- 署名はad-hoc（識別子`dev.cffnpwr.shepherdr`）とする。署名を無償の範囲で完結させる方針であり、有償のApple Developer Program（Developer ID署名・公証）は使わない。
- バンドルIDと配置先が同一の差し替えであれば、バイナリが変わってもLNP承認は維持される。ad-hoc署名では識別の安定した追跡は保証されないため（[全体アーキテクチャ](./architecture.md)）、更新後に接続が拒否された場合の再許可を[運用](./operations.md)で定める。

## リリースと配布

- GitHub Actionsでタグからリリースビルドを生成し、GitHub Releasesに添付する。
- 配布は`cffnpwr/homebrew-tap`のcaskで行う。
- quarantine属性が付いたad-hoc署名のappはGatekeeperに拒否される。cask経由の取得で属性が付くかはtap実装時に確認し、付く場合はquarantine解除の手順（`--no-quarantine`等）の案内をtap側で持つ。

## 開発時フロー

- 通常の開発は`cargo tauri dev`で行う。
- LNP承認は`/Applications`に配置したappバンドルに対して成立するため、LANアクセスを伴う実サービス込みのE2E確認はインストール済みビルドで行う。
