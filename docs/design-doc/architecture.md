# 全体アーキテクチャ

## 背景: ローカルネットワークプライバシー（LNP）

macOSのLNP（TCC `kTCCServiceLocalNetwork`）は、LANへのアクセスを「責任アプリ」単位で承認する。
前提となる事実は以下のとおり。

- Daemon（グローバルセッション）はLNP対象外、ユーザセッションのAgentは対象かつ承認可能である（[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)、[Apple Developer Forums 763753](https://developer.apple.com/forums/thread/763753)）。
- ad-hoc署名の素のCLIバイナリ（appバンドルでないもの）をLaunchAgentで起動した場合、承認プロンプトは出ず、システム設定→ローカルネットワークの一覧にも掲載されない。承認を与える手段が無く、LAN接続は既定拒否のまま固定される。
- ad-hoc署名のappバンドルを`/Applications`に置き、`open`（LaunchServices）でAquaセッションに起動してLANへ接続すると、承認プロンプトが出て一覧にも掲載される。署名証明書は不要である。
- LNPはプログラムの識別をcode signatureで追跡し、ad-hoc署名では識別の安定した追跡は保証されない（[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)）。ただし、バンドルIDと配置先が同一の差し替えであれば、バイナリが変わっても承認は維持される。

典型的な被害例が、LaunchAgent起動の`herdr server`のペイン内からLANホストへssh不可（`EHOSTUNREACH`）となる問題である（同系統の未解決報告: [herdr discussion #1137](https://github.com/ogulcancelik/herdr/discussions/1137)）。

LNP対象外であるDaemon（グローバルセッション）でサービスを動かす経路は採らない。
メニューバー常駐UIはユーザのGUIセッション（Aqua）を前提としており、gui domain外で動くDaemonでは成立しないためである。

## 全体構成

Shepherdr.appはRust＋Tauri製のメニューバー常駐appとする。
設定ファイル`~/.config/shepherdr/config.toml`のサービス定義を読み、各サービスを子プロセスとして起動・監視する（[サービス管理](./service-management.md)）。
Dockには表示せず、トレイとログウィンドウだけをUIとして持つ（[UI](./ui.md)）。

```text
launchd (gui domain)
 └─ open /Applications/Shepherdr.app   ← LaunchAgent plist
     └─ Shepherdr.app                  ← LNPの責任アプリ・メニューバー常駐
         ├─ サービス1（config.tomlの定義からspawn）
         │   └─ 子孫プロセス
         └─ サービス2 ...
```

- バンドルID: `dev.cffnpwr.shepherdr`
- 配置先: `/Applications/Shepherdr.app`
- 署名: ad-hoc（[ビルドと配布](./distribution.md)）

## 責務分界

| 構成要素 | 責務 | 管理場所 |
| --- | --- | --- |
| Shepherdr.app | サービスのspawn・監視・再起動、トレイ操作、ログ捕捉と閲覧 | 本リポジトリ |
| `config.toml` | サービス定義 | dotfiles |
| LaunchAgent plist | ログイン時にappを`open`で1回起動する | dotfiles |
| Homebrew tap | リリース物の配布とquarantine解除の案内 | `cffnpwr/homebrew-tap` |
| LNP承認 | システム設定→プライバシーとセキュリティ→ローカルネットワークでの許可 | 手動（初回） |

サービスの操作は責任コード=Shepherdr.appに帰属し、承認はapp全体に記録される（[設計原則](./principles.md)）。
多段の子孫のLAN接続・`exec`置換を挟んだ経路・親プロセス終了後の孤児の新規接続は、いずれもappへの許可1つで通り、LNP承認は「Shepherdr」1エントリで全サービスを賄える。
サービスの追加・変更は`config.toml`の編集のみで完結し、再承認は不要である。

## app自体の起動と終了

- サポートする起動経路は`open`（LaunchServices）経由のみとする。自動起動はLaunchAgentが`open /Applications/Shepherdr.app`を実行する形とする。
- appのクラッシュからの復帰機構はapp側には持たない。クラッシュ後は次回の起動まで無復帰となり、その間サービスは孤児として稼働を続け、次回起動時にクリーンアップされた後、設定に従って起動し直される（[サービス管理](./service-management.md)）。
- 多重起動はアプリ内ガード（単一インスタンス制御）で抑止する。`open`は`-n`を付けない限り既存インスタンスがあれば新規起動しないが、バイナリ直接実行など非サポート経路で起動された場合も多重化だけは防ぐ。
