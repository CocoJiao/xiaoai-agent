#!/bin/sh
#
# 静音窗真机验收（issue #6 第一步·外部播放期间抑制采集）：把「四条证据」变成退出码。
#
# 用法（在设备上跑）：
#   ./verify-quiet-window.sh start   # 记下日志当前位移，打印要做的三件事
#   （按提示和音箱交互）
#   ./verify-quiet-window.sh check   # 只检查 start 之后新增的那段日志
#
# 为什么按位移而不是整份日志：agent.log 是长期追加的，整份 grep 会把上周那条
# `end=max_duration` 也算进来，「验过了」就成了假象。
#
# 只用 busybox 里有的东西（awk / grep / tail / wc / sort）。设备上没有 pgrep、
# 没有 bash 数组，所以这里也不用（见 docs/flash-troubleshooting.md 第五节）。
set -u

AGENT_LOG=${XIAOAI_AGENT_LOG:-/data/open-xiaoai/xiaoai-agent.log}
STATE=${XIAOAI_VERIFY_STATE:-/tmp/xiaoai-verify-quiet.offset}
SLICE=${XIAOAI_VERIFY_SLICE:-/tmp/xiaoai-verify-quiet.slice}

fails=0
pass() { printf 'PASS  %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; fails=$((fails + 1)); }
note() { printf '      %s\n' "$*"; }

# grep -c 在 0 命中时退出码是 1，`|| true` 是为了不把 0 变成空串。
count() { grep -c "$1" "$SLICE" 2>/dev/null || true; }

case "${1:-check}" in
start)
	[ -r "$AGENT_LOG" ] || { echo "读不到日志：$AGENT_LOG" >&2; exit 2; }
	offset=$(wc -c < "$AGENT_LOG" | tr -d ' ')
	printf '%s\n' "$offset" > "$STATE"
	printf '已记录日志位移：%s 字节（%s）\n\n' "$offset" "$AGENT_LOG"
	cat <<'TXT'
接下来按顺序做三件事，全部做完再跑 ./verify-quiet-window.sh check

  1) 短回答：问一句，让它把答案念完，**不要**打断。
  2) 打断：再问一句，答案念到一半时喊唤醒词（顺便确认 1 秒内停住）。
  3) 长回答：问一句答案超过 25 秒的，播完看它能不能接下一句。

前提：tts.provider 是外部 TTS（native 不开静音窗，验不出东西）；
      RUST_LOG 至少 info（默认就是）；prompt 里别让它答得太短。
TXT
	;;
check)
	[ -r "$AGENT_LOG" ] || { echo "读不到日志：$AGENT_LOG" >&2; exit 2; }
	[ -f "$STATE" ] || { echo "先跑一次：$0 start" >&2; exit 2; }
	offset=$(cat "$STATE")
	tail -c "+$((offset + 1))" "$AGENT_LOG" > "$SLICE" 2>/dev/null || : > "$SLICE"

	muted=$(count 'CAPTURE_MUTED')
	unmuted=$(count 'CAPTURE_UNMUTED')
	settled=$(count '静音窗收紧到尾巴')
	capped=$(count '没等到 EndReached')
	cut=$(count 'timed out waiting for user speech')
	echo_utt=$(grep 'CAPTURE_UTTERANCE' "$SLICE" 2>/dev/null | grep -c 'end=max_duration' || true)

	printf '本轮新增日志 %s 行（从位移 %s 起）\n\n' "$(wc -l < "$SLICE" | tr -d ' ')" "$offset"

	# 1) 静音窗真的开过：provider / 开关 / 播放命令是不是外部那条路。
	if [ "$muted" -ge 1 ]; then
		pass "静音窗开过 $muted 次（外部播放确实抑制了采集）"
	else
		bad "一次 CAPTURE_MUTED 都没有：provider 还是 native？还是 suppress_capture_while_playing 关了？"
	fi

	# 2) 窗口是被「事实」收紧的，不是跑到兜底上限。这条是整套设计的地基。
	if [ "$settled" -ge 1 ]; then
		pass "按 EndReached 收窗 $settled 次（走的是事实，不是兜底上限）"
	else
		bad "没有一次按 EndReached 收窗：窗口全靠兜底上限，等于每次回答后静到上限（几十秒）"
	fi

	# 3) 打断不该留下「没等到 EndReached」的告警。
	if [ "$capped" -eq 0 ]; then
		pass "没有「没等到 EndReached」告警（打断被正确识别，没有空转到上限）"
	else
		bad "出现 $capped 次「没等到 EndReached」：打断没被识别，先确认设备上 pidof 可用"
	fi

	# 4) 回声没有进 VAD：否则录音会一路到 max_utterance。
	if [ "$echo_utt" -eq 0 ]; then
		pass "没有录音跑到 end=max_duration（回声没进 VAD）"
	else
		bad "有 $echo_utt 段录音跑到 end=max_duration：回声还在进 VAD"
	fi

	# 5) 长回答没把采集判死（这一条同时盯 mipns 会不会自己关流）。
	if [ "$cut" -eq 0 ]; then
		pass "没有「timed out waiting for user speech」（长回答没被判死）"
	else
		bad "出现 $cut 次「timed out waiting for user speech」：采集被判死了，查 mipns 是否自己关流"
	fi

	# 6) 每一对 MUTED→UNMUTED 的时长：能看出是「跟着播放收」还是「跑满上限」。
	intervals=$(grep -E 'CAPTURE_(MUTED|UNMUTED)' "$SLICE" 2>/dev/null | awk '
		BEGIN { pv = -1 }
		function secs(ts, a, b, c) {
			if (ts !~ /T/) return -1
			split(ts, a, "T")
			split(a[2], b, ":")
			if (b[3] == "") return -1
			split(b[3], c, ".")
			return b[1] * 3600 + b[2] * 60 + c[1] + ("0." c[2])
		}
		/CAPTURE_UNMUTED/ { if (pv >= 0) { now = secs($1); if (now >= 0) printf "%.1f\n", now - pv } pv = -1; next }
		/CAPTURE_MUTED/   { pv = secs($1) }
	')

	# 这一条只报数不判定：判据在 2)，这里给人一个「有多接近兜底上限」的直觉。
	if [ -n "$intervals" ]; then
		note "静音窗时长（秒）：$(printf '%s' "$intervals" | tr '\n' ' ')"
		note "对照那次回答的长度看：≈ 音频时长 + 3s 说明跟着播放收；"
		note "接近「宽限 + 2×时长 + 10s」说明吃的是兜底上限（配合 2) 一起看）。"
	else
		note "没有成对的 CAPTURE_MUTED/UNMUTED（或时间戳格式不认识）：间隔请人工核对"
	fi

	echo
	if [ "$fails" -eq 0 ]; then
		printf '全部通过。把这段输出贴进 PR #34 的评论就够收尾了。\n'
		exit 0
	fi
	printf '%s 项未通过，见上面的 FAIL。\n' "$fails"
	exit 1
	;;
*)
	echo "用法：$0 [start|check]" >&2
	exit 2
	;;
esac
