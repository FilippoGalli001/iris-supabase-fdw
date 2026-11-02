ARG IMAGE=intersystemsdc/iris-community:latest
FROM $IMAGE

WORKDIR /home/irisowner/dev
COPY --chown=irisowner:irisowner . .

RUN iris start IRIS && \
    iris session IRIS < /home/irisowner/dev/iris.script && \
    iris stop IRIS quietly
