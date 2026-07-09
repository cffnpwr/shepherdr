# 運用

導入・更新・アンインストールの継続的な運用手順を示す。
LaunchAgent plistと`config.toml`の配置はdotfilesの責務であり、ここでは扱わない。

## 導入

1. `cffnpwr/homebrew-tap`のcaskでインストールする（quarantine解除を含む手順はtapの案内に従う）。
2. dotfilesを適用し、LaunchAgent plistと`config.toml`を配置する。
3. appを起動し、システム設定→プライバシーとセキュリティ→ローカルネットワークで「Shepherdr」を許可する（LANアクセスの初回試行時にプロンプトも出る）。

## 更新

1. caskを更新する。
2. バイナリが変わるとLNP承認がリセットされるため、ローカルネットワークで再度許可する（[ビルドと配布](./distribution.md)）。

## アンインストール

1. トレイからShepherdrを終了する。
2. caskをアンインストールし、dotfilesからplistと`config.toml`の配置を外す。
3. システム設定→ローカルネットワークに残る「Shepherdr」エントリは手動で削除する（LocalNetworkサービスは`tccutil`で確実にリセットできない）。

## ログの確認

- ログウィンドウ（[UI](./ui.md)）から閲覧する。
- ファイルは`~/Library/Logs/shepherdr/<name>.log`にあり、Console.appや任意のツールでも読める。
