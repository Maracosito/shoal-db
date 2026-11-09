# Example app image. Build from the repository root:
#   docker build -f examples/compose-app/Dockerfile -t shoal/example-app:dev .
FROM python:3.12-slim

WORKDIR /app

# Install the local Shoal Python SDK from source.
COPY sdks/python /opt/shoal-python
RUN pip install --no-cache-dir /opt/shoal-python

COPY examples/compose-app/embed.py examples/compose-app/seed.py examples/compose-app/app.py /app/
COPY examples/compose-app/data /app/data

RUN useradd --system --uid 10001 demo
USER demo

EXPOSE 8000
CMD ["python", "app.py"]
