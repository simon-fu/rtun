#!/bin/bash

# 显示命令
# set -x

# 获取本脚本所在目录
CURR_DIR="$( cd "$( dirname $0)" && pwd )"

cd $CURR_DIR
$CURR_DIR/hysteria-darwin-arm64 client -c client.yaml
