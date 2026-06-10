FROM python:3.12-slim

WORKDIR /fixtures

COPY tests/fixtures/requirements.txt ./requirements.txt
RUN pip install --no-cache-dir -r requirements.txt

COPY tests/fixtures/protocol_fixtures.py ./protocol_fixtures.py

EXPOSE 18080 18443 18081 19090

CMD ["python", "/fixtures/protocol_fixtures.py"]
