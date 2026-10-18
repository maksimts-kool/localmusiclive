# The page viewers open (e.g. a Samsung TV's browser) and the live stream it
# plays, on its own port so the control panel can stay private.
FROM nginx:1.29-alpine
ARG HLSJS_VERSION=1.6.13
ADD --chmod=644 https://cdn.jsdelivr.net/npm/hls.js@${HLSJS_VERSION}/dist/hls.min.js /usr/share/nginx/html/hls.min.js
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
