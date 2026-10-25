#!/bin/sh
#
# 静音窗真机验收（issue #6 第一步·外部播放期间抑制采集）：把证据变成退出码。
#
# 用法（在设备上跑；scp 过去记得先 chmod +x，scp 不保留执行位）：
#   ./verify-quiet-window.sh start   # 记下日志当前位移，打印前提体检和要做的三件事
#   （按提示和音箱交互）
#   ./verify-quiet-window.sh check   # 只检查 start 之后新增的那段日志
#
# 为什么按位移而不是整份日志：agent.log 是长期追加的，整份 grep 会把上周那条
# `end=max_duration` 也算进来，「验过了」就成了假象。
#
# 为什么 check 还要先核位移有没有失效：日志被重置/被 `>` 重写过之后片段是空的，
# 而「没有 X」这种负向断言在空片段上恒真——最坏的情况是人看到几条 PASS，
# 就当成「回声没进来」。
#
# 只用 busybox 里有的东西（awk / grep / tail / wc / sort）。设备上没有 pgrep、
# 没有 bash 数组，所以这里也不用（见 docs/flash-troubleshooting.md 第五节）。
set -u

AGENT_LOG=${XIAOAI_AGENT_LOG:-/data/open-xiaoai/xiaoai-agent.log}
AGENT_CONFIG=${XIAOAI_AGENT_CONFIG:-/data/open-xiaoai/agent.yaml}
STATE=${XIAOAI_VERIFY_STATE:-/tmp/xiaoai-verify-quiet.offset}
SLICE=${XIAOAI_VERIFY_SLICE:-/tmp/xiaoai-verify-quiet.slice}
# 三次交互怎么也有几十行；少于这个数说明交互没做完，结论不可信。
MIN_LINES=${XIAOAI_VERIFY_MIN_LINES:-20}

fails=0
pass() { printf 'PASS  %s\n' "$*"; }
bad() { printf 'FAIL  %s\n' "$*"; fails=$((fails + 1)); }
note() { printf '      %s\n' "$*"; }

# grep -c 在 0 命中时退出码是 1，`|| true` 是为了不把 0 变成空串。
count() { grep -c "$1" "$SLICE" 2>/dev/null || true; }

# 打失败现场：匹配行前后各几行，最多 max 行。只打不判。
# 不用 grep -A/-B：上下文选项要 busybox 编译时开，设备上不一定有；
# awk 读两遍同一份片段就够了（FNR == NR 是第一遍，只记行号）。
ctx() { # $1=模式 $2=前 $3=后 $4=最多几行
	awk -v pat="$1" -v b="$2" -v a="$3" -v max="$4" '
		FNR == NR {
			if ($0 ~ pat) { lo[++k] = FNR - b; hi[k] = FNR + a }
			next
		}
		{
			for (i = 1; i <= k; i++) {
				if (FNR >= lo[i] && FNR <= hi[i]) {
					if (++shown > max) exit
					print "      | " $0
					next
				}
			}
		}
	' "$SLICE" "$SLICE" 2>/dev/null
}

# 读 tts 段下的一个键（只看顶层缩进，避免把 asr.provider / llm.provider 混进来）。
#
# 必须裁掉行内注释和引号：`provider: native  # 临时换回验 AEC` 和
# `provider: "native"` 是现场最常见的两种写法，而前提体检的全部价值就在那
# 一刻——值里带着注释尾巴，判断就静默失效了。顺序不能反：先裁注释再删 key
# 再去引号，否则 `provider: openai  # "x"` 的尾巴会把引号处理削坏。
cfg() {
	[ -r "$AGENT_CONFIG" ] || return 0
	awk -v key="$1" '
		/^tts:/ { t = 1; next }
		/^[^[:space:]#]/ { t = 0 }
		t && $0 ~ "^[[:space:]]+" key ":" {
			v = $0
			sub(/[[:space:]]+#.*$/, "", v)
			sub("^[[:space:]]+" key ":[[:space:]]*", "", v)
			sub(/^["\047]/, "", v)
			sub(/["\047]$/, "", v)
			print v
			exit
		}
	' "$AGENT_CONFIG" 2>/dev/null
}

# 前提体检：验完才发现 provider 是 native 是最冤的返工。
precheck() {
	printf '前提体检（%s）：\n' "$AGENT_CONFIG"
	if [ ! -r "$AGENT_CONFIG" ]; then
		note "读不到配置文件，下面按「用默认值」假设——自己核一遍 tts 段"
		return 0
	fi
	provider=$(cfg provider)
	playback=$(cfg playback_command)
	suppress=$(cfg suppress_capture_while_playing)
	printf '  provider = %s\n' "${provider:-（没写，默认 native）}"
	printf '  playback_command = %s\n' "${playback:-（没写 = 默认播放器，这是验收要求的配置）}"
	printf '  suppress_capture_while_playing = %s\n' "${suppress:-（没写，默认 true）}"

	[ -n "$provider" ] && [ "$provider" != "native" ] \
		|| note "!! provider 是 native 或没写：native 走 pns 有 AEC，根本不开静音窗，这里验不出任何东西"
	[ -z "$playback" ] \
		|| note "!! playback_command 是自定义的：看不到播放器日志，窗口只能按时长估，断言 2 必然 FAIL、断言 3 恒 PASS。验收请先注释掉它用默认播放器"
	[ "$suppress" != "false" ] \
		|| note "!! suppress_capture_while_playing=false：静音窗被关掉了，断言 1 必然 FAIL"
}

case "${1:-check}" in
start)
	[ -r "$AGENT_LOG" ] || { echo "读不到日志：$AGENT_LOG" >&2; exit 2; }
	offset=$(wc -c < "$AGENT_LOG" | tr -d ' ')
	printf '%s\n' "$offset" > "$STATE"
	printf '已记录日志位移：%s 字节（%s）\n\n' "$offset" "$AGENT_LOG"

	precheck
	echo
	cat <<'TXT'
接下来按顺序做三件事，全部做完再跑 ./verify-quiet-window.sh check

  1) 短回答：问一句，让它把答案念完，**不要**打断。
  2) 打断：再问一句，答案念到一半时喊唤醒词（顺便确认 1 秒内停住）。
  3) 长回答：问一句答案超过 25 秒的，播完看它能不能接下一句。

前提：tts.provider 是外部 TTS（native 不开静音窗，验不出东西）；
      tts.playback_command 保持默认（自定义命令必 FAIL 断言 2）；
      RUST_LOG 至少 info（默认就是）；prompt 里别让它答得太短。

check 通过 ≠ 三件事都验过了：只做第 1 件也能全 PASS。所以通过后请把
USER_ASR 的原文和这段输出一起贴出来——「1 秒内停住」只有人能验。
TXT
	;;
check)
	[ -r "$AGENT_LOG" ] || { echo "读不到日志：$AGENT_LOG" >&2; exit 2; }
	[ -f "$STATE" ] || { echo "先跑一次：$0 start" >&2; exit 2; }
	offset=$(cat "$STATE")

	# 位移失效检查：日志被重置 / 换了文件时，负向断言会在空片段上全 PASS。
	# 顺序必须是「先核大小、再切片段、再核行数、最后才跑断言」：反了就会先在
	# 空片段上攒出几条 PASS 再退出，那是比报错更坏的一种。
	# 注意位移是绝对字节数：只适用于「只追加」的日志。设备上的 agent.log 目前
	# 就是只追加；若是「复制+截断」式轮转，新文件会很快重新超过 offset，这道
	# 门槛挡不住。
	size=$(wc -c < "$AGENT_LOG" | tr -d ' ')
	if [ "$size" -lt "$offset" ]; then
		echo "日志被重置过或换了文件（当前 $size 字节 < 记录的位移 $offset）——" >&2
		echo "片段是空的，「没有 X」会全部假 PASS。请重新 start 再验一次。" >&2
		exit 2
	fi
	tail -c "+$((offset + 1))" "$AGENT_LOG" > "$SLICE" 2>/dev/null || : > "$SLICE"

	lines=$(wc -l < "$SLICE" | tr -d ' ')
	if [ "$lines" -lt "$MIN_LINES" ]; then
		echo "片段只有 $lines 行（少于 $MIN_LINES）：三次交互不可能这么少。" >&2
		echo "确认三件事都做了（或 XIAOAI_VERIFY_MIN_LINES=10 $0 check 调门槛）再 check。" >&2
		exit 2
	fi

	muted=$(count 'CAPTURE_MUTED')
	unmuted=$(count 'CAPTURE_UNMUTED')
	settled=$(count '静音窗收紧到尾巴')
	capped=$(count '没等到 EndReached')
	failed=$(count 'turn failed:')
	idle=$(count 'session idle timeout')
	echo_utt=$(grep 'CAPTURE_UTTERANCE' "$SLICE" 2>/dev/null | grep -c 'end=max_duration' || true)

	printf '本轮新增日志 %s 行（从位移 %s 起）\n\n' "$lines" "$offset"

	playback=$(cfg playback_command)

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
		[ -z "$playback" ] && note "（playback_command 是默认的，所以这条 FAIL 是真问题）" \
			|| note "（playback_command 是自定义的：这条路径设计上就看不到播放器日志，必然 FAIL，不是回声漏了）"
	fi

	# 3) 打断不该留下「没等到 EndReached」的告警。前提是默认播放器——自定义命令
	#    根本不 spawn 等待任务，这条恒 PASS，所以下面单独提示。
	if [ "$capped" -eq 0 ]; then
		pass "没有「没等到 EndReached」告警（打断被正确识别，没有空转到上限）"
	else
		bad "出现 $capped 次「没等到 EndReached」：打断没被识别，先确认设备上 pidof 可用"
	fi
	[ -z "$playback" ] || note "!! playback_command 是自定义的：没有等待任务，这条 PASS 不构成证据"

	# 4) 回声没有进 VAD：否则录音会一路到 max_utterance。但上限也可能是别的声源，
	#    别把归因写死——AGENTS.md 记着 provider: native 也出过 end=max_duration。
	if [ "$echo_utt" -eq 0 ]; then
		pass "没有录音跑到 end=max_duration（回声没进 VAD）"
	else
		bad "有 $echo_utt 段录音跑到 end=max_duration（录到上限才停）：可能是回声，也可能是房间里别的声源——对照紧随其后的 USER_ASR 原文和 peak 判断，别直接去调采集阈值"
		note "现场（匹配行 + 其后 6 行）："
		ctx 'end=max_duration' 0 6 12
	fi

	# 5) 整轮答案没作废。原来查的 `timed out waiting for user speech` 在今天的代码里
	#    恒不出现（is_capture_timeout 把它改写成 info "session idle timeout"，
	#    原文只可能经由 error "turn failed: {err:?}" 出去，而空闲超时那条到不了那里），
	#    所以改成查回声三连的最后一环：答案被当成提问 → 整轮失败。
	#
	#    **末尾那个冒号是载荷，不是排版**：唤醒打断的收尾也打 "turn failed"
	#    （main.rs 的 InterruptCause::Failure("turn failed")），但它渲染成
	#    `cause="turn failed"`，不带冒号；只有 error!("turn failed: {err:?}")
	#    带冒号。谁把日志改成 `turn failed - ...`，这条断言就会静默恒真——
	#    正是上一轮那条断言翻的车，别再翻一次。
	if [ "$failed" -eq 0 ]; then
		pass "没有整轮失败（turn failed）：回声没演变成「答案被当成提问」"
	else
		bad "出现 $failed 次 turn failed：整轮答案作废了——可能是回声被当成提问后 LLM 失败，也可能是网络/ASR 服务故障，对照紧随其后的 USER_ASR 原文判断"
		note "现场（匹配行前后各 3 行）："
		ctx 'turn failed:' 3 3 14
	fi

	# 只报数不判定：三步之间停顿超过 session_idle_timeout_s 也会出现，属正常。
	[ "$idle" -eq 0 ] || note "session idle timeout 出现 $idle 次：三步之间停顿超过 session_idle_timeout_s 也会这样，不算失败"

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
		printf '全部通过。把这段输出连同 USER_ASR 原文贴进 PR 评论就够收尾了。\n'
		[ -z "$playback" ] || printf '注意：playback_command 是自定义的，断言 2/3 在本次配置下不可判读，别当证据。\n'
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
