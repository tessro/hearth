FROM python:3-alpine

WORKDIR /srv/public
COPY public/ /srv/public/

EXPOSE 8000
CMD ["python3", "-m", "http.server", "8000", "--bind", "0.0.0.0"]
