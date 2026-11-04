ARG IMAGE=intersystemsdc/iris-community:latest
FROM $IMAGE

WORKDIR /home/irisowner/dev
COPY --chown=irisowner:irisowner . .

# iris.script does the whole setup at build time (gateway, demo data, users).
# Writing iris.init tells the image entrypoint that the instance is already initialized:
# otherwise its first-boot hook runs, can fail (dbapi.connect error on recent images) and
# shuts IRIS down right after the first start, leaving a ~15 s window where IRIS is up.
RUN iris start IRIS && \
    iris session IRIS < /home/irisowner/dev/iris.script && \
    iris stop IRIS quietly && \
    date > "${ISC_PACKAGE_INSTALLDIR:-/usr/irissys}/iris.init"
