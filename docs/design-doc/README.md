# Shepherdr Design Doc

任意のCLIサービスを、macOSのローカルネットワークプライバシー（LNP）で承認可能なメニューバー常駐appの配下で起動・監視するサービスマネージャの設計。

## スコープと非スコープ

### スコープ

- Shepherdr.app本体（サービスの起動・監視・トレイ操作・ログ閲覧）
- 設定ファイル（`config.toml`）の仕様
- ビルド・リリース・配布の方式
- 導入・更新・アンインストールの継続運用

### 非スコープ

- 配布用tap（`cffnpwr/homebrew-tap`）の実装
- LaunchAgent plistと`config.toml`の配置・管理
- 包む各サービス（herdr等）自体の実装・設定

## 目次

| ドキュメント | 内容 |
| --- | --- |
| [設計原則](./principles.md) | 全体を通しての原則 |
| [全体アーキテクチャ](./architecture.md) | LNPの前提事実、全体構成、責務分界 |
| [サービス管理](./service-management.md) | 設定スキーマ、spawn方式、監視・再起動、リロード、ログ |
| [UI](./ui.md) | トレイメニュー、ログウィンドウ、フロントエンド構成、デザイン方針 |
| [ビルドと配布](./distribution.md) | ビルド、署名、リリース、Homebrew tap、開発時フロー |
| [運用](./operations.md) | 導入、更新、アンインストール |
