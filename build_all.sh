#! /bin/sh
cargo build
cd test_c_service
clang -o test_service test_service.c $(pkg-config libsystemd --libs)