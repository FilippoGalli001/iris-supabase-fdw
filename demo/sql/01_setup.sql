-- 1. enable the Wrappers extension and the Wasm FDW handler
create extension if not exists wrappers with schema extensions;

create foreign data wrapper wasm_wrapper
  handler extensions.wasm_fdw_handler
  validator extensions.wasm_fdw_validator;

-- 2. connect to IRIS through the iris-fdw-gateway REST package
create server iris_server
  foreign data wrapper wasm_wrapper
  options (
    -- local development build (copied into the db container by demo/load-wasm.sh)
    fdw_package_url 'file:///tmp/iris_fdw.wasm',
    fdw_package_name 'fgalli:iris-fdw',
    fdw_package_version '0.1.0',

    api_url 'http://iris-fdw:52773',
    username 'fdw_user',
    password 'fdw_password',
    debug 'true'
  );

create schema if not exists iris;
