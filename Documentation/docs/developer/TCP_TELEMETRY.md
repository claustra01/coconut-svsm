# ゲストLinuxを経由するTCPログの送信

このブランチは `sock-trawl`（`990fc4fe`）を基点としています。SVSMが観測したTCP接続を
キューに格納し、ゲストLinuxのmiscドライバが実験用SVSMプロトコルを通じてまとめて
読み出します。ゲスト内のPythonプロセスが各レコードをUDPデータグラムとして転送します。
vsockデバイスやQEMUのソース変更は不要です。

`make TCP_LOG_MODE=guest` でSVSMをビルドし、生成したIGVMイメージを既存の `run.sh` で
使用します。ネットワークには既存のゲスト用virtio-net-pciを使用できます。

**VM内で動作するLinuxカーネル**に、
`tools/tcp-guest-linux/linux-6.11-socktrawl.patch` の補助関数を追加する必要があります。
パッチは上流のLinux v6.11を基にしています。実際に使うcoconutゲストカーネルに適用可能か
確認してください。ホスト上の `uname -r` だけでは、ゲストのカーネルバージョンは判断できません。

```sh
# ゲストカーネルのソースツリーで実行する
git apply --check /path/to/coconut-svsm/tools/tcp-guest-linux/linux-6.11-socktrawl.patch
git apply /path/to/coconut-svsm/tools/tcp-guest-linux/linux-6.11-socktrawl.patch
# CONFIG_AMD_MEM_ENCRYPT=y でカーネルを再ビルド・インストールし、そのカーネルでVMを起動する
```

パッチ適用済みカーネルのビルドツリーと `Module.symvers` を使い、ゲスト内で付属モジュールを
ビルドして読み込みます。

```sh
make -C tools/tcp-guest-linux KDIR=/path/to/patched-linux-build
sudo insmod tools/tcp-guest-linux/socktrawl.ko
```

QEMUホストで受信スクリプトを起動します。

```sh
python3 scripts/tcp-udp-receiver.py --bind 127.0.0.1 --port 4050
```

続いてゲスト内で中継スクリプトを起動します。提示されたQEMUのユーザーネットワーク設定では、
`10.0.2.2` がホストを指します。

```sh
sudo python3 scripts/tcp-guest-relay.py --host 10.0.2.2 --port 4050
```

受信スクリプトはJSONを出力します。中継スクリプトは、ノンブロッキングの
`/dev/socktrawl` にデータがない場合、1秒間隔で再度読み取りを試みます
（`--interval` で変更可能）。1回の読み取りで最大73件の56バイトレコードを取り出し、
キューが空ならEAGAINを返します。読み取りプロセスは1つだけにしてください。
読み取り用バッファはゲスト専用メモリのままで、ホストとの共有メモリには変換しません。

プロトコル番号 `0x80000054` は、この実装用の実験的な番号です。標準として割り当てられた
番号ではありません。呼び出し番号0では、RCXにページ境界へ整列した出力先GPA、
RDXに56〜4096バイトの容量を渡します。RCXにはコピーしたバイト数が返り、必ず56の倍数に
なります。コアプロトコルの問い合わせにはバージョン1を返します。読み出したイベントは
キューから削除され、再取得や受信確認はできません。
Linux 6.11のSVSM呼び出し処理はSEV実装の内部関数なので、カーネルパッチで
`snp_socktrawl_read()` を公開します。

レコード形式は `virtio-vsock` と同じCTCPです。配送はベストエフォートです。
生成側キューは128件で、UDP通信でもレコードが失われる場合があります。
レコード内の破棄件数は、生成側キューの容量超過だけを数えます。
中継スクリプトにはTLSやディスクへの一時保存を実装していません。
ゲストを停止すると転送も停止します。

開発環境では、`svsm` と `stage2` を `x86_64-unknown-none` 向けにクロスビルドし、
リンクまで確認しました。`CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER` にRustツールチェーンの
`ld.lld` を指定し、`tcp-log-guest` のみを有効にしています。
IGVM/vTPMを含むイメージ全体のビルドは実施していません。確認用コマンドは次のとおりです。

```sh
cargo check --locked --offline -p svsm --target x86_64-unknown-none --features tcp-log-guest
rustc --edition=2024 --test kernel/src/vmm/tcp_event.rs -o /tmp/tcp-event-tests
/tmp/tcp-event-tests
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests -v
```

パッチが上流のLinux v6.11に適用できることは確認済みです。ゲスト用モジュールのビルドと
実際のSVSM呼び出しは、パッチ適用済みのゲストカーネル環境で確認する必要があります。
開発ホストには、そのLinuxビルドツリーとSNP VMがないため未検証です。
