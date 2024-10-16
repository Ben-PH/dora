import os
from dora import Node
from transformers import Qwen2VLForConditionalGeneration, AutoProcessor
from qwen_vl_utils import process_vision_info
import numpy as np
import pyarrow as pa
from PIL import Image

DEFAULT_PATH = "Qwen/Qwen2-VL-2B-Instruct"
CUSTOM_MODEL_PATH = os.getenv("CUSTOM_MODEL_PATH", DEFAULT_PATH)
DEFAULT_QUESTION = os.getenv(
    "DEFAULT_QUESTION",
    "Describe this image",
)
ADAPTER_PATH = os.getenv("ADAPTER_PATH", "")

# Check if flash_attn is installed
try:
    import flash_attn as _

    model = Qwen2VLForConditionalGeneration.from_pretrained(
        CUSTOM_MODEL_PATH,
        torch_dtype="auto",
        device_map="auto",
        attn_implementation="flash_attention_2",
    )
except (ImportError, ModuleNotFoundError):
    model = Qwen2VLForConditionalGeneration.from_pretrained(
        CUSTOM_MODEL_PATH,
        torch_dtype="auto",
        device_map="auto",
    )


if ADAPTER_PATH != "":
    model.load_adapter(ADAPTER_PATH, "dora")


# default processor
processor = AutoProcessor.from_pretrained(CUSTOM_MODEL_PATH)


def generate(frames: dict, question):
    """
    Generate the response to the question given the image using Qwen2 model.
    """

    messages = [
        {
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "image": image,
                }
                for image in frames.values()
            ]
            + [
                {"type": "text", "text": question},
            ],
        }
    ]

    # Preparation for inference
    text = processor.apply_chat_template(
        messages, tokenize=False, add_generation_prompt=True
    )
    image_inputs, video_inputs = process_vision_info(messages)
    inputs = processor(
        text=[text],
        images=image_inputs,
        videos=video_inputs,
        padding=True,
        return_tensors="pt",
    )
    inputs = inputs.to("cuda")

    # Inference: Generation of the output
    generated_ids = model.generate(**inputs, max_new_tokens=128)
    generated_ids_trimmed = [
        out_ids[len(in_ids) :]
        for in_ids, out_ids in zip(inputs.input_ids, generated_ids)
    ]
    output_text = processor.batch_decode(
        generated_ids_trimmed,
        skip_special_tokens=True,
        clean_up_tokenization_spaces=False,
    )
    return output_text[0]


def main():
    pa.array([])  # initialize pyarrow array
    node = Node()

    question = DEFAULT_QUESTION
    frames = {}

    for event in node:
        event_type = event["type"]

        if event_type == "INPUT":
            event_id = event["id"]

            if "image" in event_id:
                storage = event["value"]
                metadata = event["metadata"]
                encoding = metadata["encoding"]
                width = metadata["width"]
                height = metadata["height"]

                if encoding == "bgr8":
                    channels = 3
                    storage_type = np.uint8
                elif encoding == "rgb8":
                    channels = 3
                    storage_type = np.uint8
                else:
                    raise RuntimeError(f"Unsupported image encoding: {encoding}")

                frame = (
                    storage.to_numpy()
                    .astype(storage_type)
                    .reshape((height, width, channels))
                )
                if encoding == "bgr8":
                    frame = frame[:, :, ::-1]  # OpenCV image (BGR to RGB)
                elif encoding == "rgb8":
                    pass
                else:
                    raise RuntimeError(f"Unsupported image encoding: {encoding}")
                frames[event_id] = Image.fromarray(frame)

            elif event_id == "tick":
                if len(frames.keys()) == 0:
                    continue
                response = generate(frames, question)
                node.send_output(
                    "tick",
                    pa.array([response]),
                    {},
                )

            elif event_id == "text":
                text = event["value"][0].as_py()
                if text != "":
                    question = text
                if len(frames.keys()) == 0:
                    continue
                # set the max number of tiles in `max_num`
                response = generate(frames, question)
                node.send_output(
                    "text",
                    pa.array([response]),
                    {},
                )

        elif event_type == "ERROR":
            raise RuntimeError(event["error"])


if __name__ == "__main__":
    main()
