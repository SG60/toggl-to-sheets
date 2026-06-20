FROM alpine:latest@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b AS ca-certificates
RUN apk add -U --no-cache ca-certificates

FROM scratch
ARG RUST_TARGET_DIR 

# Move the ca-certs across (required for tls to work)
COPY --from=ca-certificates /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

COPY ${RUST_TARGET_DIR}/toggl-to-sheets /

CMD [ "/toggl-to-sheets" ]

