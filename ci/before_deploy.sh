#!/bin/sh

mkdir stage

cp target/$TARGET/release/ruuvitag-upload stage

cd stage

tar cvzf ../ruuvitag-upload-${TRAVIS_TAG}-${TARGET}.tar.gz *

cd ..

rm -rf stage
