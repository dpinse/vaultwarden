#!/usr/bin/env bash

# The default Debian-based images support these arches for all database backends.
arches=(
    amd64
    arm64
)
export arches

if [[ "${DOCKER_TAG}" == *alpine ]]; then
    distro_suffix=.alpine
fi
export distro_suffix
