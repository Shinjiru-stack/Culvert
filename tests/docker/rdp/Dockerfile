FROM ubuntu:latest

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        sudo \
        xrdp \
        xorgxrdp \
        xfce4 \
        xfce4-terminal \
        dbus-x11 \
        x11-xserver-utils \
    && rm -rf /var/cache/apt/* \
    && (useradd -m -s /bin/bash ubuntu 2>/dev/null || true) \
    && echo 'ubuntu:ubuntu' | chpasswd \
    && usermod -aG sudo,adm ubuntu \
    && echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu

EXPOSE 3389

# sesman (session manager) in background, xrdp listener in the foreground.
CMD ["bash", "-c", "(xrdp-sesman || service xrdp-sesman start) && exec xrdp -n"]
