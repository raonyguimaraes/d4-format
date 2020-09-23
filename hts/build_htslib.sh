#!/bin/bash

set -ex

pushd ${OUT_DIR}

if [ -z ${HTSLIB_VERSION} ]; then
	HTSLIB_VERSION="1.9"
fi

rm -rf ${OUT_DIR}/htslib

git clone -b ${HTSLIB_VERSION} https://github.com/samtools/htslib.git

cd htslib

cat > config.h << CONFIG_H
#define HAVE_LIBBZ2 1
#define HAVE_DRAND48 1
CONFIG_H

if [ ! -z $(echo ${TARGET} | grep musl) ]
then
	sed -i 's/gcc/musl-gcc/g' Makefile

	curl 'https://zlib.net/zlib-1.2.11.tar.gz' | tar xz
	cd zlib-1.2.11
	CC=musl-gcc ./configure
	make
	cp libz.a ..
	cd ..
	
	curl https://pilotfiber.dl.sourceforge.net/project/bzip2/bzip2-1.0.6.tar.gz | tar xz
	cd bzip2-1.0.6
	sed -i 's/gcc/musl-gcc/g' Makefile
	make
	cp libbz2.a ..
	cd ..

	sed -i 's/CPPFLAGS =/CPPFLAGS = -Izlib-1.2.11 -Ibzip2-1.0.6/g' Makefile

	make -j8 lib-static

	exit 0
fi


if [ "${HTSLIB}" = "static"  ]
then
	make -j8 lib-static
else
	make -j8 lib-shared
fi
