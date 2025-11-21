# Use an official Python runtime as a parent image
FROM python:3.12-slim

# Set the working directory in the container
WORKDIR /app

# Copy the requirements file into the container at /app
COPY requirements.txt .

# Install any needed packages specified in requirements.txt
RUN pip install --no-cache-dir -r requirements.txt

# Create a non-root user and switch to it
RUN useradd -m botuser && chown -R botuser /app
USER botuser

# Copy the current directory contents into the container at /app
COPY --chown=botuser:botuser bot.py .

# Run bot.py when the container launches
CMD ["python", "bot.py"]