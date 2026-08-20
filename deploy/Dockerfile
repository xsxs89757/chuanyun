# 穿云服务端镜像
#
# 构建：docker build -f deploy/Dockerfile -t chuanyun-server .
# 运行：docker run -d --name chuanyun \
#         -p 7000:7000 -p 127.0.0.1:7080:7080 \
#         -v /etc/chuanyun:/etc/chuanyun -v chuanyun-data:/var/lib/chuanyun \
#         chuanyun-server

FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# rustls 走 ring provider、rusqlite 走 bundled，musl 下无需系统库
RUN cargo build --release -p cy-server --bin chuanyun-server

FROM alpine:3
RUN apk add --no-cache ca-certificates tzdata \
    && adduser -D -H -u 10001 chuanyun
COPY --from=builder /build/target/release/chuanyun-server /usr/local/bin/chuanyun-server

USER chuanyun
VOLUME ["/var/lib/chuanyun"]
EXPOSE 7000 7080

ENTRYPOINT ["/usr/local/bin/chuanyun-server"]
CMD ["run", "-c", "/etc/chuanyun/server.toml"]
