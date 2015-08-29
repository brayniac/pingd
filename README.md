# pingd - rust mio ping server

pingc is a rust implementation of an ASCII PING/PONG server which uses the mio library for async network io

[![Build Status](https://travis-ci.org/brayniac/pingd.svg?branch=master)](https://travis-ci.org/brayniac/pingd)
[![License](http://img.shields.io/:license-mit-blue.svg)](http://doge.mit-license.org)

## Build

To use `pingd`, with rust installed, clone and cd into this folder:

```shell
cargo build --release;
```

## Usage

[pingc](https://github.com/brayniac/pingc) is an example client

```shell
./target/release/pingd --listen [HOST:PORT]
```

## Features

* utilize mio for async network io
