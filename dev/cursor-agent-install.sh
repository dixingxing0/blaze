#!/usr/bin/env bash
#
# Cloud Agent install script for Blaze.
#
# Prepares a Cloud Agent VM to build and run Blaze end to end:
#   - installs the JDK 8 / Maven / protoc toolchain that the default image lacks
#     (Rust nightly and a C toolchain are already provided by the base image and
#     pinned via rust-toolchain.toml),
#   - makes Java 8 the default (Blaze targets Java 1.8 bytecode),
#   - builds the fat JAR for the spark-3.5 shim, which also compiles the native
#     Rust engine (libblaze.so) and warms the Cargo/Maven caches.
#
# The script is idempotent: re-running it re-uses installed packages and warm
# build caches, and only rebuilds the native library when its sources change
# (build-native.sh is checksum-guarded).

set -euo pipefail

# Spark shim + build mode used to validate the environment. `pre` builds fast
# and is what CI uses for tests; switch MODE to `release` for optimized builds.
SHIM="${SHIM:-spark-3.5}"
MODE="${MODE:-pre}"

JAVA8_HOME="/usr/lib/jvm/java-8-openjdk-amd64"

echo "==> Installing system toolchain (JDK 8, Maven, protoc)"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y -qq openjdk-8-jdk maven protobuf-compiler

echo "==> Selecting Java 8 as the default JDK"
sudo update-alternatives --install /usr/bin/java java "${JAVA8_HOME}/bin/java" 2000
sudo update-alternatives --install /usr/bin/javac javac "${JAVA8_HOME}/bin/javac" 2000
sudo update-alternatives --set java "${JAVA8_HOME}/bin/java"
sudo update-alternatives --set javac "${JAVA8_HOME}/bin/javac"

# Persist JAVA_HOME for interactive agent shells (idempotent: file is rewritten).
sudo tee /etc/profile.d/blaze-java.sh >/dev/null <<EOF
export JAVA_HOME="${JAVA8_HOME}"
EOF
export JAVA_HOME="${JAVA8_HOME}"

echo "==> Toolchain versions"
java -version
mvn -version | head -1
protoc --version
rustc --version

echo "==> Building Blaze (${SHIM} / ${MODE}) — compiles native engine + fat JAR"
cd "$(dirname "$0")/.."
mvn -B package -P"${SHIM}" -P"${MODE}" -DskipTests

echo "==> Build complete. Artifacts:"
ls -la target/blaze-engine-"${SHIM}"-"${MODE}"-*-SNAPSHOT.jar
ls -la target/"${MODE}"/libblaze.so
