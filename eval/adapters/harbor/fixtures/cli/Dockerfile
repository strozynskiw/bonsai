ARG BASE_IMAGE=bonsai-tb4-base:local
FROM ${BASE_IMAGE}
USER root
ENTRYPOINT []
WORKDIR /workspace
COPY fake-bonsai fake-dsh fake-node b3sum /opt/benchmark/bin/
RUN chmod 755 /opt/benchmark/bin/* && ln -s /opt/benchmark/bin/fake-node /usr/local/bin/node
ENV PATH="/opt/benchmark/bin:/usr/local/bin:/usr/bin:/bin"
CMD ["sh", "-c", "sleep infinity"]
