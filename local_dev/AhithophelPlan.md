# Project Ahithophel

Knowledge is power. There is a vast amount of knowledge available, but people are fallible—even the most intelligent individuals can make mistakes. Errors often occur not from a lack of knowledge, but because the right action slips the mind at a critical moment.

The aim of this project is to provide a "genius advisor" available 24/7 to observe your life, understand your personal context, and advise you on business, personal matters, and more.

## Plan for our first Advanced Agent that utilizes The Ahithophel Framework

1. **Data Collection**
   Gather data about the user's life, preferences, and knowledge. 
   - For the initial launch, the user will start with a prompt describing their situation. If the context is clear and actionable, the system will proceed to the answering phase. If not, Ahithophel will ask follow-up questions to build a complete understanding before generating advice.
   - **Future Plans:** Data collection will eventually expand significantly. We plan to utilize cameras and audio recordings to observe the user's life and build a detailed data profile. This will include third-party integrations connecting to the user's social media profiles. If a query involves other individuals, the system will gather public data on them as well, allowing it to evaluate the situation comprehensively and provide highly contextual advice.

2. **Knowledge Base**
   We must curate specific books and resources for the AI to draw from.
   - We will start with a single foundational book. The AI will determine which chapters are relevant to the user's query and apply those principles. However, because concepts in one chapter often rely on another, the system must avoid "tunnel vision" and ensure it doesn't lock itself too strictly to a single chapter, which could cause it to miss important real-world applications.
   - **Future Plans:** Expand the knowledge base to include behavioral psychology and temperament. For example, once the primary agent determines a plan of action, a secondary "Executor Agent" can refine and tailor that plan specifically to the personalities of the people involved.

## Agent Architecture and Roles

### Data Collection Agents
1. **Min-Info Agent:** Determines if the user has provided the bare minimum information required to operate. 
   - *Example:* If the user says, "I walked into my house," the system recognizes this is insufficient and prompts for more detail. If the user says, "I walked into my house and found my wife in bed with another man," the situation is clear-cut, and the system moves to the next phase.
2. **Yenta Agent:** Once the minimum information threshold is met, this agent determines if further context is needed. It asks targeted follow-up questions to deepen its understanding. 
   - *Example:* In the scenario above, it might ask, "How long have you been married?", "Do you have children?", "What is your current financial situation?", or "What is your emotional state?"
3. **Message Refiner Agent:** Takes the user's finalized input, condenses it, corrects any spelling or grammar mistakes, and passes a clean, refined prompt to the Answering phase.

### Answering Agents
4. **Traffic Controller Agent:** Analyzes the refined question and routes it to the correct books or chapters within the knowledge base.
5. **Answer Agent:** Takes the Traffic Controller's suggested context and drafts an initial answer.
6. **Answer Controller Agent:** Evaluates the draft to verify that it actually answers the user's core question.
7. **Answer Refiner Agent:** Edits the answer to be clear and concise. Before delivering it to the user, it passes the draft back to the Traffic Controller Agent to see if the proposed solution triggers the need for *new* books/chapters. This feedback loop can run up to a maximum of 3 to 5 times.
8. **Temperament Agent:** Once a plan of action is determined, this agent refines the advice to be more tailored to the personalities and temperaments of the individuals involved.

### Memory Agents
9. **Memory Agent:** Takes the final Q&A and stores it so it can be easily accessed in the future. This builds a persistent, long-term profile of the user's life and the advice given, helping the system identify behavioral patterns over time.
10. **Memory Retrieval Agent:** Responsible for surfacing relevant past context when a new question is asked. 
   - *Example:* If the user asks, "What should I do about my marriage?", this agent retrieves past interactions regarding their spouse and feeds them to the Traffic Controller to ensure the new advice aligns with historical context.

### Research Agent
11. **Research Agent:** Allows the system to ask questions of *itself* rather than the user. If the core knowledge base is insufficient, this agent can independently research scientific literature or external data to open up new avenues of thought. This prevents the system from getting stuck in local minimums, ensures advice is backed by up-to-date research, and expands the AI's problem-solving capabilities.


## PROJECT Hushai (The data intake system)

the idea for Hushai, just like in the Bible story, is a double agent. A double agent gathers information with an unparalleled ability to hear and see everything.

the idea here for this app is to help us gather as much information as possible by having a mobile application with its camera and voice recording on and capturing every detail

the key is in every detail. we will need a way to process all this information
we will need to create a Rust server back end. the back end's responsibility is to ingest this massive amount of data and ingest it in a way that can be used to analyze the person and their relationships to better help them

all the data will be stored, video will be analyzed for visual scenario categorization, and objects will be created with data points such as the timestamp, person, action, vibes, disposition, and more if available
the audio will be transcribed, split into sentences with timestamps, and embedded

to get a bit more into the disposition bit there will need to be an AI that can analyze every bit of the data. now there might be an AI model that can transcribe and understand the audio's emotional context as well, such as if the person is angry, down, happy, etc.


> Note: every few hours or number of words will need to be summarized and embedded so that any top future-level queries can perhaps reference it, kind of like a book, so that we don't need to summarize the entire day every time. We have an agent that can benefit from it.



### Post data intake

once all the data is in we can then have multiple agents working in parallel
such as the above first idea where it focuses on the "Yes!" books approach and others to figure out the best way to help the target person with really anything they need help with, business, relationships, and more. Now that the AI has the full context, it can actually help





### Privacy Privacy Privacy

the backend will use all local models running locally on the user's computer, and the app will be sending the audio and video through a local server for now to avoid things getting on the web
we will need the best and fastest local models for speech to text, for embeddings, for video recognition, to recognize people, and more..




# Actionable steps (described in terms of different teams working in parallel)
in order to put this all together we will need a modular approach to development and to the system architecture as a whole,
so that the cameras for example or should we call it data intake can be many different types from dashcam/gopro style cameras or any other camera can be used for the intake and to pass on that data to the backend server in a standardized format the backend doesn't care about the intake method so so perhaps this is a project for itself to create a standardized data format to intake data from different cameras audio and video but that can be a separate task that a team can work on in parallel to the backend server development and the agent development, so we can have multiple teams working on different parts of the system at the same time and then we can integrate it all together once we have the different pieces ready.
# Our general tech stack is A rust backend and a native android mobile application for the camera and audio everything else will be using the rust ecosystem for the backend and the AI models and the agents and everything else.
- first we have the audio and video part of the app that can capture continuous audio and video and stream it to a server as well as store it locally if unable to stream then only to upload the local data when the connection is back.. there will be quite a bit of overlap/cordination needed between the app development team and the backend server team to make sure that the data is being sent in a format that the backend can understand and process efficiently.

- the backend server will need to be able to handle the massive amount of data being sent to it from multiple sources ie multiple cameras.
- when working with multiple sources it would be nice to be able to glue the cameras together so that the AI can better understand the context of the situation, for example if one camera is in the living room and another camera is in the kitchen and they both capture the same event from different angles, it would be nice to be able to combine that data to get a better understanding of the situation.
this task is from one team since there is a lot of work to be done on this end from identifying overlapping data frames such as that done in military drones to create a wider image and put together the images of the same event from different angles...
1. store the data in a structured format that can be easily queried and analyzed.
2. Have queues that process the incoming data, to elaborate on how this should work we will have say 3 services that need to process the incoming data to then write to the main database. 1. audio transcription and sentiment analysis 2. audio embedding 3. video analysis
all of them will take a different amount of time to process.70j



# Tasks to do now:
1.[✅] create a create issue claude skill that create a github issue in a specific format 
the format is as follows:
- **Title**: [Project Name] - [Task Description]
- **Description**: A detailed description of the task, including any relevant information, requirements,
- **Acceptance Criteria**: at what point is this task complete?
- **How to Test**: how can we test to make sure the task is complete and working as expected, part of the format is to test the real thing some how or another but it must be tested not just with unit or integration tests but with the real thing as well, so we can be sure it works in the real world and not just in a test environment.


2. [] come up with a real plan for how the backed is going to ingest the camera data from at least 2 sources at the same time and from different camera sources as well the first will be the android application and the second will be a simple webcam setup the idea is to start with two camera sources so that we make sure that the intake system is modular enough to handle multiple sources and not get to stuck on just one type of camera since its crucial part of the program to overtime, expand the amount of intake cameras we have to enlarge array of different types once we have a clear plan we can start writing up the tickets for the initial mobile app and the initial backend server


3. [] Create the ticket to create the native adroid application and the axum rust backend server that will intake the data and store it in a database.