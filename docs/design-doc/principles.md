# 設計原則

Shepherdr全体に共通する設計上の原則を定める。
個々の設計判断はこれらの原則に従う。

## 責任アプリを維持する

サービスは子プロセスとしてspawnし、自プロセスを`exec`で置換しない。
macOSはローカルネットワーク操作を行ったプロセスの「責任コード（responsible code）」を追跡し、appがspawnしたヘルパーの操作はappに帰属して、承認はapp全体に記録される（[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)）。
Shepherdr.appを親として全サービスを起動することで、サービスとその子孫（サーバ→shell→`ssh`等）の操作を責任コード=Shepherdr.appに帰属させる。
帰属はspawn時に決まり、子孫が`exec`で置換されても、親プロセスが終了した後の新規接続でも維持されることを実機で確認済みである。
自プロセスを`exec`で置換すると、コード識別が置換先のバイナリに変わり、以後spawnするサービスの帰属先がShepherdr.appでなくなるため行わない。

## 暗黙に介在しない

launchdの`ProgramArguments`と同様に、シェルを暗黙に挟まず`command`のargvをそのまま実行する。
ログイン環境（PATH等）が必要なサービスは`login_shell`フラグで明示的に宣言し、宣言された介在だけをshepherdrがargvを保ったまま行う（[サービス管理](./service-management.md)）。
宣言されていない変換を行わないことで、shepherdrが加える介在はすべて設定の記述から読める。

## 設定ファイルを唯一の真実とする

appの起動時状態は常に`config.toml`から導出し、トレイからの実行時操作は永続化しない。
恒久的な変更はすべて設定ファイルの編集で表現する。
