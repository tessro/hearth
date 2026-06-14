FROM python:3-alpine

WORKDIR /srv/public
COPY public/ /srv/public/
COPY serve-http.sh /usr/local/bin/serve-http
RUN chmod 0755 /usr/local/bin/serve-http

EXPOSE 8000
CMD ["serve-http"]
