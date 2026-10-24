#!/usr/bin/env bash

set -e

BASE_DIR=$(pwd)
WORK_DIR=$BASE_DIR/temp

# 打包前清掉 patch 可能留下的 .orig（--no-backup-if-mismatch 已经能防止新增，
# 这里兜住历史残留）。**必须在 mksquashfs 之前**执行，否则备份文件已经被打进镜像。
find "$WORK_DIR" -name '*.orig' -delete 2>/dev/null || true

FIRMWARE=$(basename $(ls $BASE_DIR/assets/*.bin 2>/dev/null | head -n 1) .bin)

cd $WORK_DIR

if [ ! -f "$BASE_DIR/assets/$FIRMWARE.bin" ]; then
    echo "❌ 固件文件不存在，请先下载固件到：$BASE_DIR/assets/"
    exit 1
fi

if [ ! -d "$FIRMWARE" ]; then
    echo "❌ 解压后的固件文件夹不存在，请先提取固件"
    exit 1
fi

SQUASHFS_INFO=$(file $FIRMWARE/root.squashfs)
echo "🚗 原始固件信息: $SQUASHFS_INFO"

COMPRESSION=$(echo "$SQUASHFS_INFO" | grep -o "xz\|gzip\|lzo\|lz4\|zstd compressed" | cut -d' ' -f1)
BLOCKSIZE=$(echo "$SQUASHFS_INFO" | grep -o "blocksize: [0-9]* bytes" | cut -d' ' -f2)

echo "🔥 使用原始参数重新打包固件..."
# 参数必须与原厂超级块一致（2026-10-24 用 unsquashfs -s 核对 OH2P 1.62.2 原厂镜像）：
#   Compression xz / Block size 131072 / Filesystem is exportable via NFS /
#   Always-use-fragments option is not specified / Xattrs are not stored
# 所以这里**不能**加 -no-exports（会变成不可 export）或 -always-use-fragments
# （原厂没开）。加了这两个参数，超级块与原厂不一致。
mksquashfs squashfs-root $FIRMWARE/root-patched.squashfs \
    -comp $COMPRESSION -b $BLOCKSIZE \
    -noappend -all-root -no-xattrs \
    -p "dev/console c 600 0 0 5 1"


# 校验固件大小上限
MODEL=$(cat $BASE_DIR/assets/.model)
IMAGE_MAX_SIZE=0
if [ "$MODEL" = "OH2P" ]; then
    IMAGE_MAX_SIZE=$((0x02800000))
else
    echo "❌ 暂不支持当前设备型号: $MODEL。本项目目前只在 OH2P 上测试成功。"
    exit 1
fi

# 取文件字节数，跨平台：
#   - GNU coreutils / busybox: stat -c %s
#   - BSD / macOS:             stat -f %z（或 gstat -c %s）
# 只用其中一个会在另一个平台上取到空值或多行文件系统信息，后面 $(( )) 直接报错。
file_size_bytes() {
    _f="$1"
    if command -v gstat >/dev/null 2>&1; then
        gstat -L -c %s "$_f" 2>/dev/null && return 0
    fi
    if stat -c %s "$_f" >/dev/null 2>&1; then
        stat -c %s "$_f"
        return 0
    fi
    if stat -f %z "$_f" >/dev/null 2>&1; then
        stat -f %z "$_f"
        return 0
    fi
    wc -c < "$_f" | tr -d ' '
}

SIZE=$(file_size_bytes "$FIRMWARE/root-patched.squashfs")
case "$SIZE" in
    '' | *[!0-9]*)
        echo "❌ 无法读取固件大小（得到: ${SIZE:-空}），中止构建" >&2
        exit 1
        ;;
esac
SIZE_MB=$((SIZE / 1024 / 1024))
IMAGE_MAX_SIZE_MB=$((IMAGE_MAX_SIZE / 1024 / 1024))

echo "📊 当前固件大小: $SIZE 字节 ($SIZE_MB MB)"

if [ "$SIZE" -ge "$IMAGE_MAX_SIZE" ]; then
    echo "❌ 固件大小超过允许的最大值：$IMAGE_MAX_SIZE 字节 ($IMAGE_MAX_SIZE_MB MB)"
    exit 1
fi

REMAINING_BYTES=$((IMAGE_MAX_SIZE - SIZE))
REMAINING_MB=$((REMAINING_BYTES / 1024 / 1024))
echo "✅ 固件大小检查通过，剩余可用空间: $REMAINING_BYTES 字节 ($REMAINING_MB MB)"

rm -rf $BASE_DIR/assets/$FIRMWARE
# 只复制最终产物：整目录复制会把上一次构建的残留（temp/ 中间文件、旧镜像）
# 一起带进 assets/，后续刷机工具可能误用。
mkdir -p $BASE_DIR/assets/$FIRMWARE
cp -f $FIRMWARE/root-patched.squashfs $BASE_DIR/assets/$FIRMWARE/

echo "✅ 打包完成，固件文件已复制到 assets 目录..."
echo $BASE_DIR/assets/$FIRMWARE/root-patched.squashfs
