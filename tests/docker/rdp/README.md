// Test RDP server for verifying Culvert's RDP launcher (mstsc hand-off).
//
// Old xrdp releases (0.9.x, e.g. the scottyhardy/docker-remote-desktop image
// this replaces) emit malformed license packets during negotiation, which
// Windows mstsc mis-reports as "you already have a console session in
// progress". This image runs a current xrdp so the handshake is clean:
//
//     docker build -t culvert-rdp tests/docker/rdp
//     docker run -d --name testrdp --restart unless-stopped -p 3389:3389 culvert-rdp
//
// Logins: ubuntu / ubuntu (sudo-capable XFCE desktop).
