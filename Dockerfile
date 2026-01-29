FROM alpine:latest@sha256:25109184c71bdad752c8312a8623239686a9a2071e8825f20acb8f2198c3f659 AS ca-certificates
RUN apk add -U --no-cache ca-certificates

FROM scratch
ARG RUST_TARGET_DIR 

# Move the ca-certs across (required for tls to work)
COPY --from=ca-certificates /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

COPY ${RUST_TARGET_DIR}/toggl-to-sheets /

CMD [ "/toggl-to-sheets" ]

