# nix shell nixpkgs#apksigner nixpkgs#objection nixpkgs#aapt nixpkgs#apktool
set -euo pipefail

PATH=$PWD/nix-workaround:$PATH objection patchapk -s ./patched.apk --skip-signing --architecture arm64-v8a
# output into patched.objection.apk
rm -rf tmpfrida
apktool d patched.objection.apk -o tmpfrida --no-src
apktool b tmpfrida/ -o patched.objection.aligned.apk --use-aapt1

echo "signing"
apksigner sign --ks keys.keystore --ks-key-alias mytestkey --ks-pass pass:android patched.objection.aligned.apk
echo "verifying"
apksigner verify patched.apk
mv patched.objection.aligned.apk patched-with-objection.apk
