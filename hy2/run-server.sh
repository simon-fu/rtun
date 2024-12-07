#!/bin/bash

# 显示命令
# set -x

# 获取本脚本所在目录
CURR_DIR="$( cd "$( dirname $0)" && pwd )"

CERT_FILE=$CURR_DIR/self_signed_certs/cert.pem
KEY_FILE=$CURR_DIR/self_signed_certs/key.pem
CFG_FILE=$CURR_DIR/server_once.yaml

cd $CURR_DIR
cp $CURR_DIR/server.yaml $CFG_FILE

# sed -i "s#__tls_cert__#$CERT_FILE#g" $CFG_FILE
# sed -i "s#__tls_key__#$KEY_FILE#g" $CFG_FILE

$CURR_DIR/hysteria-darwin-arm64 server -c $CFG_FILE
