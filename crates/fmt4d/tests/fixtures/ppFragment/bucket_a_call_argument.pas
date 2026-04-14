unit BucketACallArgument;

interface

implementation

procedure Touch;
begin
  DoSomething(Now, {$IFDEF DEBUG} 1 {$ELSE} 30 {$ENDIF});
end;

end.
