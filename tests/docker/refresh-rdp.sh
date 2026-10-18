#!/bin/bash
# Rebuild + replace the test RDP container (fresh image on ubuntu:latest so
# xrdp is a current release, not the 0.9.x that ships in old images).
set -e
docker build -t culvert-rdp "$(dirname "$0")/rdp"
docker stop testrdp 2>/dev/null || true
docker rm testrdp 2>/dev/null || true
docker run -d --name testrdp --restart unless-stopped -p 3389:3389 culvert-rdp
sleep 3
docker ps --filter name=testrdp --format '{{.Names}} {{.Image}} {{.Status}}'
