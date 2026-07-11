# 全体アーキテクチャ

## 背景: ローカルネットワークプライバシー（LNP）

macOSのLNP（TCC `kTCCServiceLocalNetwork`）は、LANへのアクセスを「責任アプリ」単位で承認する。
前提となる事実は以下のとおり。

- Daemon（root・グローバルセッション）はLNP対象外、ユーザセッションのAgentは対象かつ承認可能である（[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)、[Apple Developer Forums 763753](https://developer.apple.com/forums/thread/763753)）。
- ad-hoc署名の素のCLIバイナリ（appバンドルでないもの）をLaunchAgentで起動した場合、承認プロンプトは出ず、システム設定→ローカルネットワークの一覧にも掲載されない。承認を与える手段が無く、LAN接続は既定拒否のまま固定される。
- ad-hoc署名のappバンドルを`/Applications`に置き、`open`（LaunchServices）でAquaセッションに起動してLANへ接続すると、承認プロンプトが出て一覧にも掲載される（最小テストappで実証済み。署名証明書は不要）。
- ad-hoc署名では承認がcdhash単位で記録され、バイナリが変わるとリセットされる。

典型的な被害例が、LaunchAgent起動の`herdr server`のペイン内からLANホストへssh不可（`EHOSTUNREACH`）となる問題である（同系統の未解決報告: [herdr discussion #1137](https://github.com/ogulcancelik/herdr/discussions/1137)）。

LNP対象外であるDaemon（root・グローバルセッション）でサービスを動かす経路は採らない。
サービスとその子孫がroot実行になり、ユーザセッション固有の資源（ssh鍵・keychain・`$HOME`配下の設定）を使えず、サービスが作るsocketの所有権もユーザ権限のクライアントと噛み合わなくなるためである。

## 全体構成

```text
launchd (gui domain)
 └─ open /Applications/Shepherdr.app   ← LaunchAgent plist（dotfiles管理）
     └─ Shepherdr.app                  ← LNPの責任アプリ・メニューバー常駐
         ├─ サービス1（config.tomlの定義からspawn）
         │   └─ 子孫プロセス（LNP許可を継承）
         └─ サービス2 ...
```

Shepherdr.appはRust＋Tauri製のメニューバー常駐appとする。
`~/.config/shepherdr/config.toml`のサービス定義を読み、各サービスを子プロセスとして起動・監視する（[サービス管理](./service-management.md)）。
Dockには表示せず、トレイとログウィンドウだけをUIとして持つ（[UI](./ui.md)）。

- バンドルID: `com.cffnpwr.shepherdr`
- 配置先: `/Applications/Shepherdr.app`
- 署名: ad-hoc（[ビルドと配布](./distribution.md)）

## 責務分界

| 構成要素 | 責務 | 管理場所 |
| --- | --- | --- |
| Shepherdr.app | サービスのspawn・監視・再起動、トレイ操作、ログ捕捉と閲覧 | 本リポジトリ |
| `config.toml` | サービス定義（何を動かすか） | dotfiles |
| LaunchAgent plist | ログイン時にappを`open`で1回起動する | dotfiles |
| Homebrew tap | リリース物の配布とquarantine解除の案内 | `cffnpwr/homebrew-tap` |
| LNP承認 | システム設定→プライバシーとセキュリティ→ローカルネットワークでの許可 | 手動（初回とapp更新後） |

全サービスとその子孫は同一の責任アプリ（Shepherdr.app）を継承するため、LNP承認は「Shepherdr」1エントリで全サービスを賄える。
サービスの追加・変更は`config.toml`の編集のみで完結し、appバンドルが不変である限り再承認は不要である。

## app自体の起動と終了

- 自動起動はLaunchAgent（`RunAtLoad=true`・`KeepAlive=false`）が`open /Applications/Shepherdr.app`を実行する形とする。
- `KeepAlive`を持たないため、トレイから終了したappをlaunchdが復活させることはない。appがクラッシュした場合の自動復帰も無く、メニューバーのアイコン消失で検知する。
- 多重起動はアプリ内ガード（単一インスタンス制御）で抑止する。`open`は`-n`を付けない限り既存インスタンスがあれば新規起動しないが（`man open`）、バイナリ直接実行などの経路もガードの対象に含める。
