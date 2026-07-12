# 運用

導入・更新・アンインストールの継続的な運用手順を示す。
LaunchAgent plistと`config.toml`の内容の定義は本書では扱わない。

## 導入

1. `cffnpwr/homebrew-tap`のcaskでインストールする（quarantine解除を含む手順はtapの案内に従う）。
2. LaunchAgent plistと`config.toml`を配置する。
3. appを起動し、システム設定→プライバシーとセキュリティ→ローカルネットワークで「Shepherdr」を許可する（LANアクセスの初回試行時にプロンプトも出る）。

## 更新

1. caskを更新する。バンドルIDと配置先が同一の差し替えではLNP承認は維持される。
2. 更新後にLAN接続が拒否される場合は、システム設定→ローカルネットワークで再度許可する（[ビルドと配布](./distribution.md)）。

## アンインストール

1. トレイからShepherdrを終了する。
2. caskをアンインストールし、plistと`config.toml`の配置を外す。
3. システム設定→ローカルネットワークに残る「Shepherdr」エントリを手動で削除する。

## ログの確認

- ログウィンドウ（[UI](./ui.md)）から閲覧する。
- ファイルは`~/Library/Logs/shepherdr/<name>.log`にあり、Console.appや任意のツールでも読める。
