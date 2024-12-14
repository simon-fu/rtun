

server-side
===========
wget https://github.com/apernet/hysteria/releases/download/app%2Fv2.6.0/hysteria-linux-amd64
chmod +x hysteria-linux-amd64
ln -s hysteria-linux-amd64 hysteria

./run-server.sh
rtun punch -s --ice-server stun:stun.miwifi.com:3478 --relay-to 127.0.0.1:12777



client-side
===========
rtun punch --ice-server stun:stun.miwifi.com:3478 --relay-listen 0.0.0.0:13777
./run-client.sh